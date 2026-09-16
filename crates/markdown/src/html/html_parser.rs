use std::{cell::RefCell, collections::HashMap, mem, ops::Range};

use gpui::{DefiniteLength, FontWeight, SharedString, TextAlign, px, relative};
use html5ever::{
    Attribute, LocalName, ParseOpts, local_name, parse_document, tendril::TendrilSink,
};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom};
use pulldown_cmark::{Alignment, HeadingLevel};
use stacksafe::stacksafe;

use crate::html::html_minifier::{Minifier, MinifierOptions};

#[derive(Debug, Clone, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlBlock {
    pub source_range: Range<usize>,
    pub children: Vec<ParsedHtmlElement>,
    /// How many `<details>` this block opens that its own source never closes.
    /// The form GitHub recommends separates the disclosure body with blank lines,
    /// which ends the CommonMark HTML block right after `<summary>`, so the
    /// opening tag, the body and `</details>` arrive as separate blocks.
    pub unclosed_details: usize,
    /// How many `</details>` this block has that close a `<details>` an earlier
    /// block opened. Counted apart from [`Self::unclosed_details`] because one
    /// block can do both, as `</details>` immediately followed by the next
    /// `<details>` does when no blank line separates two disclosures.
    pub unmatched_details_closes: usize,
    /// Filled in by the markdown parser once the block that closes this one is
    /// known, so the renderer can skip everything in between while collapsed.
    pub details_span: Option<HtmlDetailsSpan>,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct HtmlDetailsSpan {
    /// Start of the root block holding the `</details>`. Everything before it
    /// belongs to the disclosure body, and root blocks nest cleanly, so a
    /// collapsed disclosure can be hidden by skipping up to this offset.
    pub close_root_block_start: usize,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum ParsedHtmlElement {
    Heading(ParsedHtmlHeading),
    List(ParsedHtmlList),
    Table(ParsedHtmlTable),
    BlockQuote(ParsedHtmlBlockQuote),
    Paragraph(ParsedHtmlParagraph),
    Image(HtmlImage),
    Details(ParsedHtmlDetails),
    Rule(Range<usize>),
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlDetails {
    pub source_range: Range<usize>,
    /// Position of this disclosure among the ones in its block, in document
    /// order. Together with the block's source offset it keys the open/closed
    /// state, which has to survive the reparse that follows every edit.
    pub index: usize,
    pub open: bool,
    pub summary: HtmlParagraph,
    pub children: Vec<ParsedHtmlElement>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlParagraph {
    pub text_align: Option<TextAlign>,
    pub contents: HtmlParagraph,
}

impl ParsedHtmlElement {
    pub fn source_range(&self) -> Option<Range<usize>> {
        Some(match self {
            Self::Heading(heading) => heading.source_range.clone(),
            Self::List(list) => list.source_range.clone(),
            Self::Table(table) => table.source_range.clone(),
            Self::BlockQuote(block_quote) => block_quote.source_range.clone(),
            Self::Paragraph(paragraph) => match paragraph.contents.first()? {
                HtmlParagraphChunk::Text(text) => text.source_range.clone(),
                HtmlParagraphChunk::Image(image) => image.source_range.clone(),
            },
            Self::Image(image) => image.source_range.clone(),
            Self::Details(details) => details.source_range.clone(),
            Self::Rule(source_range) => source_range.clone(),
        })
    }
}

pub(crate) type HtmlParagraph = Vec<HtmlParagraphChunk>;

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum HtmlParagraphChunk {
    Text(ParsedHtmlText),
    Image(HtmlImage),
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlList {
    pub source_range: Range<usize>,
    pub depth: u16,
    pub ordered: bool,
    pub items: Vec<ParsedHtmlListItem>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlListItem {
    pub source_range: Range<usize>,
    pub item_type: ParsedHtmlListItemType,
    pub content: Vec<ParsedHtmlElement>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum ParsedHtmlListItemType {
    Ordered(u64),
    Unordered,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlHeading {
    pub source_range: Range<usize>,
    pub level: HeadingLevel,
    pub contents: HtmlParagraph,
    pub text_align: Option<TextAlign>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlTable {
    pub source_range: Range<usize>,
    pub header: Vec<ParsedHtmlTableRow>,
    pub body: Vec<ParsedHtmlTableRow>,
    pub caption: Option<HtmlParagraph>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlTableColumn {
    pub col_span: usize,
    pub row_span: usize,
    pub is_header: bool,
    pub children: HtmlParagraph,
    pub alignment: Alignment,
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlTableRow {
    pub columns: Vec<ParsedHtmlTableColumn>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlBlockQuote {
    pub source_range: Range<usize>,
    pub children: Vec<ParsedHtmlElement>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlText {
    pub source_range: Range<usize>,
    pub contents: SharedString,
    pub highlights: Vec<(Range<usize>, HtmlHighlightStyle)>,
    pub links: Vec<(Range<usize>, SharedString)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HtmlHighlightStyle {
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub weight: FontWeight,
    pub link: bool,
    pub oblique: bool,
    /// `<code>` and the other monospace tags, rendered with the inline code style.
    pub code: bool,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct HtmlImage {
    pub dest_url: SharedString,
    pub source_range: Range<usize>,
    pub alt_text: Option<SharedString>,
    pub width: Option<DefiniteLength>,
    pub height: Option<DefiniteLength>,
}

impl HtmlImage {
    fn new(dest_url: String, source_range: Range<usize>) -> Self {
        Self {
            dest_url: dest_url.into(),
            source_range,
            alt_text: None,
            width: None,
            height: None,
        }
    }

    fn set_alt_text(&mut self, alt_text: SharedString) {
        self.alt_text = Some(alt_text);
    }

    fn set_width(&mut self, width: DefiniteLength) {
        self.width = Some(width);
    }

    fn set_height(&mut self, height: DefiniteLength) {
        self.height = Some(height);
    }
}

#[derive(Debug, Clone)]
struct ParseHtmlNodeContext {
    list_item_depth: u16,
    /// Alignment inherited from an enclosing container such as
    /// `<div align="center">`, which has no element of its own to carry it.
    text_align: Option<TextAlign>,
    /// Text styling inherited from enclosing inline-styled containers.
    inherited_styles: Vec<HtmlHighlightStyle>,
}

impl Default for ParseHtmlNodeContext {
    fn default() -> Self {
        Self {
            list_item_depth: 1,
            text_align: None,
            inherited_styles: Vec::new(),
        }
    }
}

pub(crate) fn parse_html_block(
    source: &str,
    source_range: Range<usize>,
) -> Option<ParsedHtmlBlock> {
    let bytes = cleanup_html(source);
    let mut cursor = std::io::Cursor::new(bytes);
    let dom = parse_document(RcDom::default(), ParseOpts::default())
        .from_utf8()
        .read_from(&mut cursor)
        .ok()?;

    let mut children = Vec::new();
    parse_html_node(
        source_range.clone(),
        &dom.document,
        &mut children,
        &ParseHtmlNodeContext::default(),
    );

    let mut details_index = 0;
    assign_details_indices(&mut children, &mut details_index);

    let (unclosed_details, unmatched_details_closes) = count_details_tags(source);

    Some(ParsedHtmlBlock {
        source_range,
        children,
        unclosed_details,
        unmatched_details_closes,
        details_span: None,
    })
}

/// Walks the `<details>` and `</details>` tags of a raw HTML block in order,
/// ignoring the ones inside comments, and returns how many disclosures the block
/// leaves open and how many it closes that an earlier block opened. html5ever
/// silently closes whatever the block left open, so this is the only way to tell
/// a self-contained disclosure from one whose body continues in later blocks.
/// Matching them in order matters: a block spelled `</details>` followed by the
/// next `<details>` both closes and opens one, and the two must not cancel out.
fn count_details_tags(source: &str) -> (usize, usize) {
    // Lowercasing ASCII keeps every byte offset, so slicing stays valid.
    let lowered = source.to_ascii_lowercase();
    let mut unclosed = 0;
    let mut unmatched_closes = 0;
    let mut index = 0;

    while let Some(offset) = lowered[index..].find('<') {
        let start = index + offset;
        let rest = &lowered[start..];

        if rest.starts_with("<!--") {
            index = match rest.find("-->") {
                Some(end) => start + end + "-->".len(),
                None => lowered.len(),
            };
            continue;
        }

        if let Some(after) = rest.strip_prefix("</details") {
            if tag_name_ends_here(after) {
                if unclosed > 0 {
                    unclosed -= 1;
                } else {
                    unmatched_closes += 1;
                }
            }
        } else if let Some(after) = rest.strip_prefix("<details")
            && tag_name_ends_here(after)
        {
            unclosed += 1;
        }

        index = start + 1;
    }

    (unclosed, unmatched_closes)
}

fn tag_name_ends_here(after_name: &str) -> bool {
    match after_name.chars().next() {
        None => true,
        Some(character) => character == '>' || character == '/' || character.is_whitespace(),
    }
}

#[stacksafe]
fn assign_details_indices(elements: &mut [ParsedHtmlElement], next_index: &mut usize) {
    for element in elements {
        match element {
            ParsedHtmlElement::Details(details) => {
                details.index = *next_index;
                *next_index += 1;
                assign_details_indices(&mut details.children, next_index);
            }
            ParsedHtmlElement::BlockQuote(block_quote) => {
                assign_details_indices(&mut block_quote.children, next_index);
            }
            ParsedHtmlElement::List(list) => {
                for item in &mut list.items {
                    assign_details_indices(&mut item.content, next_index);
                }
            }
            _ => {}
        }
    }
}

fn cleanup_html(source: &str) -> Vec<u8> {
    let mut writer = std::io::Cursor::new(Vec::new());
    let mut reader = std::io::Cursor::new(source);
    let mut minify = Minifier::new(
        &mut writer,
        MinifierOptions {
            omit_doctype: true,
            collapse_whitespace: true,
            ..Default::default()
        },
    );
    if let Ok(()) = minify.minify(&mut reader) {
        writer.into_inner()
    } else {
        source.bytes().collect()
    }
}

#[stacksafe]
fn parse_html_node(
    source_range: Range<usize>,
    node: &Node,
    elements: &mut Vec<ParsedHtmlElement>,
    context: &ParseHtmlNodeContext,
) {
    match &node.data {
        NodeData::Document => {
            consume_children(source_range, node, elements, context);
        }
        NodeData::Text { contents } => {
            let contents = contents.borrow().to_string();
            if contents.trim().is_empty() {
                return;
            }
            elements.push(ParsedHtmlElement::Paragraph(ParsedHtmlParagraph {
                text_align: context.text_align,
                contents: vec![HtmlParagraphChunk::Text(ParsedHtmlText {
                    source_range,
                    highlights: Vec::default(),
                    links: Vec::default(),
                    contents: contents.into(),
                })],
            }));
        }
        NodeData::Comment { .. } => {}
        NodeData::Element { name, attrs, .. } => {
            let styles_map = extract_styles_from_attributes(attrs);
            let text_align = text_align_from_attributes(attrs, &styles_map)
                .or(if name.local.as_ref() == "center" {
                    Some(TextAlign::Center)
                } else {
                    None
                })
                .or(context.text_align);
            let mut styles = context.inherited_styles.clone();
            if let Some(style) = html_style_from_html_styles(styles_map) {
                styles.push(style);
            }

            if name.local == local_name!("img") {
                if let Some(image) = extract_image(source_range, attrs) {
                    elements.push(ParsedHtmlElement::Image(image));
                }
            } else if name.local == local_name!("p") {
                let mut paragraph = HtmlParagraph::new();
                parse_paragraph(
                    source_range,
                    node,
                    &mut paragraph,
                    &mut styles,
                    &mut Vec::new(),
                );

                if !paragraph.is_empty() {
                    elements.push(ParsedHtmlElement::Paragraph(ParsedHtmlParagraph {
                        text_align,
                        contents: paragraph,
                    }));
                }
            } else if matches!(
                name.local,
                local_name!("h1")
                    | local_name!("h2")
                    | local_name!("h3")
                    | local_name!("h4")
                    | local_name!("h5")
                    | local_name!("h6")
            ) {
                let mut paragraph = HtmlParagraph::new();
                consume_paragraph(
                    source_range.clone(),
                    node,
                    &mut paragraph,
                    &mut styles,
                    &mut Vec::new(),
                );

                if !paragraph.is_empty() {
                    elements.push(ParsedHtmlElement::Heading(ParsedHtmlHeading {
                        source_range,
                        level: match name.local {
                            local_name!("h1") => HeadingLevel::H1,
                            local_name!("h2") => HeadingLevel::H2,
                            local_name!("h3") => HeadingLevel::H3,
                            local_name!("h4") => HeadingLevel::H4,
                            local_name!("h5") => HeadingLevel::H5,
                            local_name!("h6") => HeadingLevel::H6,
                            _ => unreachable!(),
                        },
                        contents: paragraph,
                        text_align,
                    }));
                }
            } else if name.local == local_name!("ul") || name.local == local_name!("ol") {
                if let Some(list) = extract_html_list(
                    node,
                    name.local == local_name!("ol"),
                    context.list_item_depth,
                    source_range,
                ) {
                    elements.push(ParsedHtmlElement::List(list));
                }
            } else if name.local == local_name!("blockquote") {
                if let Some(blockquote) = extract_html_blockquote(node, source_range) {
                    elements.push(ParsedHtmlElement::BlockQuote(blockquote));
                }
            } else if name.local == local_name!("table") {
                if let Some(table) = extract_html_table(node, source_range) {
                    elements.push(ParsedHtmlElement::Table(table));
                }
            } else if name.local == local_name!("hr") {
                elements.push(ParsedHtmlElement::Rule(source_range));
            } else if name.local.as_ref() == "details" {
                elements.push(ParsedHtmlElement::Details(extract_html_details(
                    node,
                    attrs,
                    source_range,
                    &ParseHtmlNodeContext {
                        text_align,
                        inherited_styles: styles,
                        ..context.clone()
                    },
                )));
            } else if is_inline_tag(&name.local) {
                let mut paragraph = HtmlParagraph::new();
                parse_paragraph(
                    source_range,
                    node,
                    &mut paragraph,
                    &mut styles,
                    &mut Vec::new(),
                );

                if paragraph_has_content(&paragraph) {
                    elements.push(ParsedHtmlElement::Paragraph(ParsedHtmlParagraph {
                        text_align,
                        contents: paragraph,
                    }));
                }
            } else {
                consume_children(
                    source_range,
                    node,
                    elements,
                    &ParseHtmlNodeContext {
                        text_align,
                        inherited_styles: styles,
                        ..context.clone()
                    },
                );
            }
        }
        _ => {}
    }
}

/// Tags that carry no block structure of their own, so runs of them and of bare
/// text are gathered into a single paragraph instead of one paragraph each.
fn is_inline_tag(name: &LocalName) -> bool {
    matches!(
        name.as_ref(),
        "a" | "abbr"
            | "b"
            | "bdi"
            | "bdo"
            | "big"
            | "br"
            | "cite"
            | "code"
            | "del"
            | "dfn"
            | "em"
            | "font"
            | "i"
            | "ins"
            | "kbd"
            | "label"
            | "mark"
            | "q"
            | "s"
            | "samp"
            | "small"
            | "span"
            | "strike"
            | "strong"
            | "sub"
            | "sup"
            | "time"
            | "tt"
            | "u"
            | "var"
            | "wbr"
    )
}

fn is_inline_node(node: &Node) -> bool {
    match &node.data {
        NodeData::Text { .. } => true,
        NodeData::Element { name, .. } => is_inline_tag(&name.local),
        _ => false,
    }
}

fn paragraph_has_content(paragraph: &HtmlParagraph) -> bool {
    paragraph.iter().any(|chunk| match chunk {
        HtmlParagraphChunk::Text(text) => !text.contents.trim().is_empty(),
        HtmlParagraphChunk::Image(_) => true,
    })
}

fn extract_html_details(
    node: &Node,
    attrs: &RefCell<Vec<Attribute>>,
    source_range: Range<usize>,
    context: &ParseHtmlNodeContext,
) -> ParsedHtmlDetails {
    let mut summary = HtmlParagraph::new();
    let mut body_nodes = Vec::new();

    for child in node.children.borrow().iter() {
        let is_summary = matches!(
            &child.data,
            NodeData::Element { name, .. } if name.local.as_ref() == "summary"
        );

        if is_summary && summary.is_empty() {
            consume_paragraph(
                source_range.clone(),
                child,
                &mut summary,
                &mut context.inherited_styles.clone(),
                &mut Vec::new(),
            );
        } else {
            body_nodes.push(child.clone());
        }
    }

    let mut children = Vec::new();
    consume_nodes(source_range.clone(), &body_nodes, &mut children, context);

    ParsedHtmlDetails {
        source_range,
        index: 0,
        open: attr_value_named(attrs, "open").is_some(),
        summary,
        children,
    }
}

#[stacksafe]
fn parse_paragraph(
    source_range: Range<usize>,
    node: &Node,
    paragraph: &mut HtmlParagraph,
    highlights: &mut Vec<HtmlHighlightStyle>,
    links: &mut Vec<SharedString>,
) {
    match &node.data {
        NodeData::Text { contents } => {
            let contents = contents.borrow().to_string();
            append_paragraph_text(source_range, paragraph, &contents, highlights, links);
        }
        NodeData::Element { name, attrs, .. } => {
            if name.local == local_name!("img") {
                if let Some(image) = extract_image(source_range, attrs) {
                    paragraph.push(HtmlParagraphChunk::Image(image));
                }
            } else if name.local == local_name!("b") || name.local == local_name!("strong") {
                highlights.push(HtmlHighlightStyle {
                    weight: FontWeight::BOLD,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if name.local == local_name!("i") {
                highlights.push(HtmlHighlightStyle {
                    italic: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if name.local == local_name!("em") {
                highlights.push(HtmlHighlightStyle {
                    oblique: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if name.local == local_name!("del") {
                highlights.push(HtmlHighlightStyle {
                    strikethrough: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if name.local == local_name!("ins") {
                highlights.push(HtmlHighlightStyle {
                    underline: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if name.local == local_name!("u") {
                highlights.push(HtmlHighlightStyle {
                    underline: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if matches!(name.local.as_ref(), "s" | "strike") {
                highlights.push(HtmlHighlightStyle {
                    strikethrough: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if matches!(name.local.as_ref(), "code" | "kbd" | "samp" | "tt" | "var") {
                highlights.push(HtmlHighlightStyle {
                    code: true,
                    ..Default::default()
                });
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else if name.local == local_name!("br") {
                append_paragraph_text(
                    source_range,
                    paragraph,
                    "\n",
                    &mut Vec::new(),
                    &mut Vec::new(),
                );
            } else if name.local == local_name!("a") {
                if let Some(url) = attr_value(attrs, local_name!("href")) {
                    highlights.push(HtmlHighlightStyle {
                        link: true,
                        ..Default::default()
                    });
                    links.push(url.into());
                }
                consume_paragraph(source_range, node, paragraph, highlights, links);
            } else {
                consume_paragraph(source_range, node, paragraph, highlights, links);
            }
        }
        _ => {}
    }
}

/// Appends `contents` to the paragraph's trailing text chunk, or starts a new
/// one, attributing the pending highlights and links to the appended range.
fn append_paragraph_text(
    source_range: Range<usize>,
    paragraph: &mut HtmlParagraph,
    contents: &str,
    highlights: &mut Vec<HtmlHighlightStyle>,
    links: &mut Vec<SharedString>,
) {
    fn items_with_range<T>(
        range: Range<usize>,
        items: impl IntoIterator<Item = T>,
    ) -> Vec<(Range<usize>, T)> {
        items
            .into_iter()
            .map(|item| (range.clone(), item))
            .collect()
    }

    if let Some(text) =
        paragraph
            .iter_mut()
            .last()
            .and_then(|paragraph_chunk| match paragraph_chunk {
                HtmlParagraphChunk::Text(text) => Some(text),
                _ => None,
            })
    {
        // The minifier has already collapsed each whitespace run to one space,
        // so a space that survives right after a `<br>` is indentation the
        // browser would not show either.
        let contents = if text.contents.ends_with('\n') {
            contents.trim_start_matches(' ')
        } else {
            contents
        };
        if contents.is_empty() {
            return;
        }

        let mut new_text = text.contents.to_string();
        new_text.push_str(contents);

        text.highlights.extend(items_with_range(
            text.contents.len()..new_text.len(),
            mem::take(highlights),
        ));
        text.links.extend(items_with_range(
            text.contents.len()..new_text.len(),
            mem::take(links),
        ));
        text.contents = SharedString::from(new_text);
    } else {
        paragraph.push(HtmlParagraphChunk::Text(ParsedHtmlText {
            source_range,
            highlights: items_with_range(0..contents.len(), mem::take(highlights)),
            links: items_with_range(0..contents.len(), mem::take(links)),
            contents: contents.to_string().into(),
        }));
    }
}

fn consume_paragraph(
    source_range: Range<usize>,
    node: &Node,
    paragraph: &mut HtmlParagraph,
    highlights: &mut Vec<HtmlHighlightStyle>,
    links: &mut Vec<SharedString>,
) {
    for child in node.children.borrow().iter() {
        parse_paragraph(source_range.clone(), child, paragraph, highlights, links);
    }
}

fn parse_table_row(source_range: Range<usize>, node: &Node) -> Option<ParsedHtmlTableRow> {
    let mut columns = Vec::new();

    if let NodeData::Element { name, .. } = &node.data {
        if name.local != local_name!("tr") {
            return None;
        }

        for child in node.children.borrow().iter() {
            if let Some(column) = parse_table_column(source_range.clone(), child) {
                columns.push(column);
            }
        }
    }

    if columns.is_empty() {
        None
    } else {
        Some(ParsedHtmlTableRow { columns })
    }
}

fn parse_table_column(source_range: Range<usize>, node: &Node) -> Option<ParsedHtmlTableColumn> {
    match &node.data {
        NodeData::Element { name, attrs, .. } => {
            if !matches!(name.local, local_name!("th") | local_name!("td")) {
                return None;
            }

            let mut children = HtmlParagraph::new();
            consume_paragraph(
                source_range,
                node,
                &mut children,
                &mut Vec::new(),
                &mut Vec::new(),
            );

            let is_header = name.local == local_name!("th");

            Some(ParsedHtmlTableColumn {
                col_span: std::cmp::max(
                    attr_value(attrs, local_name!("colspan"))
                        .and_then(|span| span.parse().ok())
                        .unwrap_or(1),
                    1,
                ),
                row_span: std::cmp::max(
                    attr_value(attrs, local_name!("rowspan"))
                        .and_then(|span| span.parse().ok())
                        .unwrap_or(1),
                    1,
                ),
                is_header,
                children,
                alignment: attr_value(attrs, local_name!("align"))
                    .and_then(|align| match align.as_str() {
                        "left" => Some(Alignment::Left),
                        "center" => Some(Alignment::Center),
                        "right" => Some(Alignment::Right),
                        _ => None,
                    })
                    .unwrap_or(if is_header {
                        Alignment::Center
                    } else {
                        Alignment::None
                    }),
            })
        }
        _ => None,
    }
}

fn consume_children(
    source_range: Range<usize>,
    node: &Node,
    elements: &mut Vec<ParsedHtmlElement>,
    context: &ParseHtmlNodeContext,
) {
    let children = node.children.borrow().clone();
    consume_nodes(source_range, &children, elements, context);
}

/// Turns a container's children into block elements, gathering each run of
/// inline nodes into one paragraph. Without the grouping, `<div>a <b>b</b></div>`
/// would become one paragraph per text node and drop the `<b>` styling, since
/// only `<p>` and the headings used to build a paragraph.
fn consume_nodes(
    source_range: Range<usize>,
    nodes: &[Handle],
    elements: &mut Vec<ParsedHtmlElement>,
    context: &ParseHtmlNodeContext,
) {
    let mut index = 0;

    while index < nodes.len() {
        let Some(node) = nodes.get(index) else { break };

        if !is_inline_node(node) {
            parse_html_node(source_range.clone(), node, elements, context);
            index += 1;
            continue;
        }

        let mut paragraph = HtmlParagraph::new();
        let mut links = Vec::new();
        while let Some(node) = nodes.get(index).filter(|node| is_inline_node(node)) {
            let mut highlights = context.inherited_styles.clone();
            parse_paragraph(
                source_range.clone(),
                node,
                &mut paragraph,
                &mut highlights,
                &mut links,
            );
            index += 1;
        }

        if paragraph_has_content(&paragraph) {
            elements.push(ParsedHtmlElement::Paragraph(ParsedHtmlParagraph {
                text_align: context.text_align,
                contents: paragraph,
            }));
        }
    }
}

fn attr_value(attrs: &RefCell<Vec<Attribute>>, name: LocalName) -> Option<String> {
    attrs.borrow().iter().find_map(|attr| {
        if attr.name.local == name {
            Some(attr.value.to_string())
        } else {
            None
        }
    })
}

fn attr_value_named(attrs: &RefCell<Vec<Attribute>>, name: &str) -> Option<String> {
    attrs.borrow().iter().find_map(|attr| {
        if attr.name.local.as_ref() == name {
            Some(attr.value.to_string())
        } else {
            None
        }
    })
}

fn html_style_from_html_styles(styles: HashMap<String, String>) -> Option<HtmlHighlightStyle> {
    let mut html_style = HtmlHighlightStyle::default();

    if let Some(text_decoration) = styles.get("text-decoration") {
        match text_decoration.to_lowercase().as_str() {
            "underline" => {
                html_style.underline = true;
            }
            "line-through" => {
                html_style.strikethrough = true;
            }
            _ => {}
        }
    }

    if let Some(font_style) = styles.get("font-style") {
        match font_style.to_lowercase().as_str() {
            "italic" => {
                html_style.italic = true;
            }
            "oblique" => {
                html_style.oblique = true;
            }
            _ => {}
        }
    }

    if let Some(font_weight) = styles.get("font-weight") {
        match font_weight.to_lowercase().as_str() {
            "bold" => {
                html_style.weight = FontWeight::BOLD;
            }
            "lighter" => {
                html_style.weight = FontWeight::THIN;
            }
            _ => {
                if let Ok(weight) = font_weight.parse::<f32>() {
                    html_style.weight = FontWeight(weight);
                }
            }
        }
    }

    if html_style != HtmlHighlightStyle::default() {
        Some(html_style)
    } else {
        None
    }
}

fn parse_text_align(value: &str) -> Option<TextAlign> {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" => Some(TextAlign::Left),
        "center" => Some(TextAlign::Center),
        "right" => Some(TextAlign::Right),
        _ => None,
    }
}

fn text_align_from_styles(styles: &HashMap<String, String>) -> Option<TextAlign> {
    styles
        .get("text-align")
        .and_then(|value| parse_text_align(value))
}

fn text_align_from_attributes(
    attrs: &RefCell<Vec<Attribute>>,
    styles: &HashMap<String, String>,
) -> Option<TextAlign> {
    text_align_from_styles(styles).or_else(|| {
        attr_value(attrs, local_name!("align")).and_then(|value| parse_text_align(&value))
    })
}

fn extract_styles_from_attributes(attrs: &RefCell<Vec<Attribute>>) -> HashMap<String, String> {
    let mut styles = HashMap::new();

    if let Some(style) = attr_value(attrs, local_name!("style")) {
        for declaration in style.split(';') {
            let mut parts = declaration.splitn(2, ':');
            if let Some((key, value)) = parts.next().zip(parts.next()) {
                styles.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }
    }

    styles
}

fn extract_image(source_range: Range<usize>, attrs: &RefCell<Vec<Attribute>>) -> Option<HtmlImage> {
    let src = attr_value(attrs, local_name!("src"))?;

    let mut image = HtmlImage::new(src, source_range);

    if let Some(alt) = attr_value(attrs, local_name!("alt")) {
        image.set_alt_text(alt.into());
    }

    let styles = extract_styles_from_attributes(attrs);

    if let Some(width) = attr_value(attrs, local_name!("width"))
        .or_else(|| styles.get("width").cloned())
        .and_then(|width| parse_html_element_dimension(&width))
    {
        image.set_width(width);
    }

    if let Some(height) = attr_value(attrs, local_name!("height"))
        .or_else(|| styles.get("height").cloned())
        .and_then(|height| parse_html_element_dimension(&height))
    {
        image.set_height(height);
    }

    Some(image)
}

fn extract_html_list(
    node: &Node,
    ordered: bool,
    depth: u16,
    source_range: Range<usize>,
) -> Option<ParsedHtmlList> {
    let mut items = Vec::with_capacity(node.children.borrow().len());

    for (index, child) in node.children.borrow().iter().enumerate() {
        if let NodeData::Element { name, .. } = &child.data {
            if name.local != local_name!("li") {
                continue;
            }

            let mut content = Vec::new();
            consume_children(
                source_range.clone(),
                child,
                &mut content,
                &ParseHtmlNodeContext {
                    list_item_depth: depth + 1,
                    ..ParseHtmlNodeContext::default()
                },
            );

            if !content.is_empty() {
                items.push(ParsedHtmlListItem {
                    source_range: source_range.clone(),
                    item_type: if ordered {
                        ParsedHtmlListItemType::Ordered(index as u64 + 1)
                    } else {
                        ParsedHtmlListItemType::Unordered
                    },
                    content,
                });
            }
        }
    }

    if items.is_empty() {
        None
    } else {
        Some(ParsedHtmlList {
            source_range,
            depth,
            ordered,
            items,
        })
    }
}

fn parse_html_element_dimension(value: &str) -> Option<DefiniteLength> {
    if value.ends_with('%') {
        value
            .trim_end_matches('%')
            .parse::<f32>()
            .ok()
            .map(|value| relative(value / 100.))
    } else {
        value
            .trim_end_matches("px")
            .parse()
            .ok()
            .map(|value| px(value).into())
    }
}

fn extract_html_blockquote(
    node: &Node,
    source_range: Range<usize>,
) -> Option<ParsedHtmlBlockQuote> {
    let mut children = Vec::new();
    consume_children(
        source_range.clone(),
        node,
        &mut children,
        &ParseHtmlNodeContext::default(),
    );

    if children.is_empty() {
        None
    } else {
        Some(ParsedHtmlBlockQuote {
            children,
            source_range,
        })
    }
}

fn extract_html_table(node: &Node, source_range: Range<usize>) -> Option<ParsedHtmlTable> {
    let mut header_rows = Vec::new();
    let mut body_rows = Vec::new();
    let mut caption = None;

    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            if name.local == local_name!("caption") {
                let mut paragraph = HtmlParagraph::new();
                parse_paragraph(
                    source_range.clone(),
                    child,
                    &mut paragraph,
                    &mut Vec::new(),
                    &mut Vec::new(),
                );
                caption = Some(paragraph);
            }

            if name.local == local_name!("thead") {
                for row in child.children.borrow().iter() {
                    if let Some(row) = parse_table_row(source_range.clone(), row) {
                        header_rows.push(row);
                    }
                }
            } else if name.local == local_name!("tbody") || name.local == local_name!("tfoot") {
                for row in child.children.borrow().iter() {
                    if let Some(row) = parse_table_row(source_range.clone(), row) {
                        body_rows.push(row);
                    }
                }
            }
        }
    }

    if !header_rows.is_empty() || !body_rows.is_empty() {
        Some(ParsedHtmlTable {
            source_range,
            body: body_rows,
            header: header_rows,
            caption,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TextAlign;

    #[test]
    fn parses_html_styled_text() {
        let parsed = parse_html_block(
            "<p>Some text <strong>strong</strong> <a href=\"https://example.com\">link</a></p>",
            0..79,
        )
        .unwrap();

        assert_eq!(parsed.children.len(), 1);
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        let HtmlParagraphChunk::Text(text) = &paragraph.contents[0] else {
            panic!("expected text chunk");
        };

        assert_eq!(text.contents.as_ref(), "Some text strong link");
        assert_eq!(
            text.highlights,
            vec![
                (
                    10..16,
                    HtmlHighlightStyle {
                        weight: FontWeight::BOLD,
                        ..Default::default()
                    }
                ),
                (
                    17..21,
                    HtmlHighlightStyle {
                        link: true,
                        ..Default::default()
                    }
                )
            ]
        );
        assert_eq!(
            text.links,
            vec![(17..21, SharedString::from("https://example.com"))]
        );
    }

    #[test]
    fn parses_html_table_spans() {
        let parsed = parse_html_block(
            "<table><tbody><tr><td colspan=\"2\">a</td></tr><tr><td>b</td><td>c</td></tr></tbody></table>",
            0..91,
        )
        .unwrap();

        let ParsedHtmlElement::Table(table) = &parsed.children[0] else {
            panic!("expected table");
        };
        assert_eq!(table.body.len(), 2);
        assert_eq!(table.body[0].columns[0].col_span, 2);
        assert_eq!(table.body[1].columns.len(), 2);
    }

    #[test]
    fn parses_html_table_th_defaults_to_center() {
        let html = "<table><thead><tr><th>H1</th><th>H2</th></tr></thead><tbody><tr><td>a</td><td>b</td></tr></tbody></table>";
        let parsed = parse_html_block(html, 0..html.len()).unwrap();

        let ParsedHtmlElement::Table(table) = &parsed.children[0] else {
            panic!("expected table");
        };

        assert_eq!(table.header.len(), 1);
        for column in &table.header[0].columns {
            assert!(column.is_header);
            assert_eq!(column.alignment, Alignment::Center);
        }

        for column in &table.body[0].columns {
            assert!(!column.is_header);
            assert_eq!(column.alignment, Alignment::None);
        }
    }

    #[test]
    fn parses_html_table_explicit_align_attribute_preserved() {
        let html = "<table>\
            <thead><tr>\
                <th align=\"right\">H1</th>\
                <th align=\"left\">H2</th>\
            </tr></thead>\
            <tbody><tr>\
                <td align=\"center\">a</td>\
                <td align=\"right\">b</td>\
            </tr></tbody>\
        </table>";
        let parsed = parse_html_block(html, 0..html.len()).unwrap();

        let ParsedHtmlElement::Table(table) = &parsed.children[0] else {
            panic!("expected table");
        };

        assert_eq!(table.header[0].columns[0].alignment, Alignment::Right);
        assert_eq!(table.header[0].columns[1].alignment, Alignment::Left);
        assert_eq!(table.body[0].columns[0].alignment, Alignment::Center);
        assert_eq!(table.body[0].columns[1].alignment, Alignment::Right);
    }

    #[test]
    fn parses_html_list_as_explicit_list_node() {
        let parsed = parse_html_block(
            "<ul><li>parent<ul><li>child</li></ul></li><li>sibling</li></ul>",
            0..64,
        )
        .unwrap();

        assert_eq!(parsed.children.len(), 1);

        let ParsedHtmlElement::List(list) = &parsed.children[0] else {
            panic!("expected list");
        };

        assert!(!list.ordered);
        assert_eq!(list.depth, 1);
        assert_eq!(list.items.len(), 2);

        let first_item = &list.items[0];
        let ParsedHtmlElement::Paragraph(paragraph) = &first_item.content[0] else {
            panic!("expected first item paragraph");
        };
        let HtmlParagraphChunk::Text(text) = &paragraph.contents[0] else {
            panic!("expected first item text");
        };
        assert_eq!(text.contents.as_ref(), "parent");

        let ParsedHtmlElement::List(nested_list) = &first_item.content[1] else {
            panic!("expected nested list");
        };
        assert_eq!(nested_list.depth, 2);
        assert_eq!(nested_list.items.len(), 1);

        let ParsedHtmlElement::Paragraph(nested_paragraph) = &nested_list.items[0].content[0]
        else {
            panic!("expected nested item paragraph");
        };
        let HtmlParagraphChunk::Text(nested_text) = &nested_paragraph.contents[0] else {
            panic!("expected nested item text");
        };
        assert_eq!(nested_text.contents.as_ref(), "child");

        let second_item = &list.items[1];
        let ParsedHtmlElement::Paragraph(second_paragraph) = &second_item.content[0] else {
            panic!("expected second item paragraph");
        };
        let HtmlParagraphChunk::Text(second_text) = &second_paragraph.contents[0] else {
            panic!("expected second item text");
        };
        assert_eq!(second_text.contents.as_ref(), "sibling");
    }

    #[test]
    fn parses_paragraph_text_align_from_style() {
        let parsed = parse_html_block("<p style=\"text-align: center\">x</p>", 0..40).unwrap();
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
    }

    #[test]
    fn parses_heading_text_align_from_style() {
        let parsed = parse_html_block("<h2 style=\"text-align: right\">Title</h2>", 0..45).unwrap();
        let ParsedHtmlElement::Heading(heading) = &parsed.children[0] else {
            panic!("expected heading");
        };
        assert_eq!(heading.text_align, Some(TextAlign::Right));
    }

    #[test]
    fn parses_paragraph_text_align_from_align_attribute() {
        let parsed = parse_html_block("<p align=\"center\">x</p>", 0..24).unwrap();
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
    }

    #[test]
    fn parses_heading_text_align_from_align_attribute() {
        let parsed = parse_html_block("<h2 align=\"right\">Title</h2>", 0..30).unwrap();
        let ParsedHtmlElement::Heading(heading) = &parsed.children[0] else {
            panic!("expected heading");
        };
        assert_eq!(heading.text_align, Some(TextAlign::Right));
    }

    #[test]
    fn prefers_style_text_align_over_align_attribute() {
        let parsed = parse_html_block(
            "<p align=\"left\" style=\"text-align: center\">x</p>",
            0..50,
        )
        .unwrap();
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
    }

    fn parse(source: &str) -> ParsedHtmlBlock {
        parse_html_block(source, 0..source.len()).expect("html block should parse")
    }

    fn paragraph_text(element: &ParsedHtmlElement) -> String {
        let ParsedHtmlElement::Paragraph(paragraph) = element else {
            panic!("expected paragraph, got {element:?}");
        };
        paragraph_chunks_text(&paragraph.contents)
    }

    fn paragraph_chunks_text(paragraph: &HtmlParagraph) -> String {
        paragraph
            .iter()
            .filter_map(|chunk| match chunk {
                HtmlParagraphChunk::Text(text) => Some(text.contents.to_string()),
                HtmlParagraphChunk::Image(_) => None,
            })
            .collect()
    }

    #[test]
    fn parses_details_as_a_disclosure() {
        let parsed = parse("<details><summary>題</summary>本文</details>");

        assert_eq!(parsed.children.len(), 1);
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };
        assert!(!details.open);
        assert_eq!(paragraph_chunks_text(&details.summary), "題");
        assert_eq!(paragraph_text(&details.children[0]), "本文");
        assert_eq!(parsed.unclosed_details, 0);
        assert_eq!(parsed.unmatched_details_closes, 0);
    }

    #[test]
    fn parses_details_open_attribute() {
        let parsed = parse("<details open><summary>題</summary>本文</details>");
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };
        assert!(details.open);
    }

    #[test]
    fn parses_details_without_summary() {
        let parsed = parse("<details>本文だけ</details>");
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };
        assert!(details.summary.is_empty());
        assert_eq!(paragraph_text(&details.children[0]), "本文だけ");
    }

    #[test]
    fn parses_nested_details_with_distinct_indices() {
        let parsed = parse(
            "<details><summary>外</summary><details><summary>内</summary>中身</details></details>",
        );

        let ParsedHtmlElement::Details(outer) = &parsed.children[0] else {
            panic!("expected outer details");
        };
        assert_eq!(outer.index, 0);
        assert_eq!(paragraph_chunks_text(&outer.summary), "外");

        let ParsedHtmlElement::Details(inner) = &outer.children[0] else {
            panic!("expected inner details");
        };
        assert_eq!(inner.index, 1);
        assert_eq!(paragraph_chunks_text(&inner.summary), "内");
        assert_eq!(paragraph_text(&inner.children[0]), "中身");
    }

    #[test]
    fn marks_details_opened_and_closed_in_separate_blocks() {
        let opening = parse("<details>\n<summary>題</summary>");
        assert_eq!(opening.unclosed_details, 1);
        assert_eq!(opening.unmatched_details_closes, 0);

        let closing = parse("</details>");
        assert_eq!(closing.unmatched_details_closes, 1);
        assert_eq!(closing.unclosed_details, 0);

        let self_contained = parse("<details><summary>題</summary>本文</details>");
        assert_eq!(self_contained.unclosed_details, 0);
        assert_eq!(self_contained.unmatched_details_closes, 0);
    }

    #[test]
    fn ignores_details_tags_inside_comments() {
        let parsed = parse("<p>a</p><!-- <details> -->");
        assert_eq!(parsed.unclosed_details, 0);
        assert_eq!(parsed.unmatched_details_closes, 0);
    }

    #[test]
    fn parses_br_as_a_line_break_inside_html_paragraphs() {
        for source in [
            "<p>a<br>b</p>",
            "<p>a<br/>b</p>",
            "<p>a<BR>b</p>",
            "<p>a<br />b</p>",
        ] {
            let parsed = parse(source);
            assert_eq!(
                paragraph_text(&parsed.children[0]),
                "a\nb",
                "{source} should render a line break"
            );
        }
    }

    #[test]
    fn parses_br_inside_containers_and_details() {
        let parsed = parse("<div>a<br>b</div>");
        assert_eq!(paragraph_text(&parsed.children[0]), "a\nb");

        let parsed = parse("<details><summary>題</summary>a<br>b</details>");
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };
        assert_eq!(paragraph_text(&details.children[0]), "a\nb");
    }

    #[test]
    fn groups_inline_children_of_a_container_into_one_paragraph() {
        let parsed = parse("<div align=\"center\"><b>中央</b> です</div>");

        assert_eq!(parsed.children.len(), 1);
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
        assert_eq!(paragraph_chunks_text(&paragraph.contents), "中央 です");

        let HtmlParagraphChunk::Text(text) = &paragraph.contents[0] else {
            panic!("expected text chunk");
        };
        assert_eq!(
            text.highlights,
            vec![(
                0..6,
                HtmlHighlightStyle {
                    weight: FontWeight::BOLD,
                    ..Default::default()
                }
            )]
        );
    }

    #[test]
    fn inherits_container_alignment_into_block_children() {
        let parsed = parse("<div align=\"center\"><p>x</p><h2>y</h2></div>");

        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));

        let ParsedHtmlElement::Heading(heading) = &parsed.children[1] else {
            panic!("expected heading");
        };
        assert_eq!(heading.text_align, Some(TextAlign::Center));
    }

    #[test]
    fn parses_center_tag_as_centered() {
        let parsed = parse("<center>x</center>");
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
    }

    #[test]
    fn parses_monospace_and_underline_tags() {
        let parsed = parse("<p><kbd>Ctrl</kbd> <u>下線</u> <s>取消</s></p>");
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph");
        };
        let HtmlParagraphChunk::Text(text) = &paragraph.contents[0] else {
            panic!("expected text chunk");
        };
        assert_eq!(text.contents.as_ref(), "Ctrl 下線 取消");

        let styles = text
            .highlights
            .iter()
            .map(|(_, style)| style.clone())
            .collect::<Vec<_>>();
        assert!(styles.iter().any(|style| style.code));
        assert!(styles.iter().any(|style| style.underline));
        assert!(styles.iter().any(|style| style.strikethrough));
    }

    #[test]
    fn keeps_text_of_unsupported_block_tags() {
        let parsed = parse("<section><figure>図の説明</figure></section>");
        assert_eq!(paragraph_text(&parsed.children[0]), "図の説明");
    }

    #[test]
    fn parses_sub_and_sup_text() {
        let parsed = parse("<p>H<sub>2</sub>O x<sup>2</sup></p>");
        assert_eq!(paragraph_text(&parsed.children[0]), "H2O x2");
    }

    #[test]
    fn parses_hr_as_a_rule() {
        let parsed = parse("<div>a<hr>b</div>");
        assert!(matches!(parsed.children[1], ParsedHtmlElement::Rule(_)));
    }

    #[test]
    fn parses_table_footer_rows_as_body_rows() {
        let html =
            "<table><tbody><tr><td>a</td></tr></tbody><tfoot><tr><td>b</td></tr></tfoot></table>";
        let parsed = parse(html);
        let ParsedHtmlElement::Table(table) = &parsed.children[0] else {
            panic!("expected table");
        };
        assert_eq!(table.body.len(), 2);
    }
}
