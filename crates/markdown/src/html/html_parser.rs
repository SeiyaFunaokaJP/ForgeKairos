use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    ops::Range,
};

use gpui::{DefiniteLength, FontWeight, SharedString, TextAlign, px, relative};
use html5ever::{
    Attribute, LocalName, ParseOpts, local_name, parse_document,
    tendril::{StrTendril, TendrilSink},
    tokenizer::{
        BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
        TokenizerResult,
    },
};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom};
use pulldown_cmark::{Alignment, HeadingLevel, Options};
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
    /// The container tags of the block's source in document order, which the
    /// markdown parser pairs across blocks. GitHub READMEs leave blank lines
    /// inside `<div align="center">` so its body is parsed as markdown, which
    /// ends the CommonMark HTML block right after the opening tag.
    pub container_tags: Vec<HtmlContainerTag>,
    /// Filled in by the markdown parser for each aligned container this block
    /// opens and a later block closes, so the renderer can align the markdown
    /// in between the way the container asks.
    pub aligned_spans: Vec<HtmlAlignedSpan>,
}

/// The tags that can hold later blocks in their body. `<details>` is followed
/// too because closing it also closes every `<div>` opened inside it, the way
/// `</div>` closes a `<center>` opened inside the `<div>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HtmlContainerName {
    Div,
    Center,
    Details,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HtmlContainerTag {
    Open {
        name: HtmlContainerName,
        text_align: Option<TextAlign>,
    },
    Close(HtmlContainerName),
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct HtmlAlignedSpan {
    /// Event indices, like [`HtmlDetailsSpan::hidden_events`]: from the event
    /// after the opening block up to the start of the block that closes it.
    pub events: Range<usize>,
    pub text_align: TextAlign,
    /// How many containers were open around this one, so where aligned spans
    /// overlap the innermost one wins.
    pub nesting: usize,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct HtmlDetailsSpan {
    /// The events that make up the disclosure body, so a collapsed disclosure
    /// can be hidden by skipping them. Event indices rather than source offsets,
    /// because the body may live inside a list item or a block quote, where the
    /// closing block shares its root block with everything around it. The range
    /// always ends on a balanced boundary, so skipping it leaves the renderer's
    /// div stack intact.
    pub hidden_events: Range<usize>,
}

/// Identifies a disclosure across reparses. Source offsets move as soon as
/// anything before the block is edited, so the key is the summary's own text
/// plus how many earlier disclosures in the document share that summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct HtmlDetailsKey {
    summary_hash: u64,
    ordinal: u32,
}

pub(crate) fn html_details_key(summary: &str, ordinal: u32) -> HtmlDetailsKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    summary.hash(&mut hasher);
    HtmlDetailsKey {
        summary_hash: hasher.finish(),
        ordinal,
    }
}

impl HtmlDetailsKey {
    /// A spelling of the key for the summary row's element id, which has to stay
    /// distinct between disclosures without leaning on source offsets either.
    pub(crate) fn element_id_suffix(&self) -> String {
        format!("{}-{}", self.summary_hash, self.ordinal)
    }
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
    /// Keys the open/closed state, which has to survive the reparse that follows
    /// every edit. Assigned per block by the parser and renumbered across the
    /// whole document by [`assign_details_ordinals`].
    pub key: HtmlDetailsKey,
    pub open: bool,
    pub summary: HtmlParagraph,
    pub children: Vec<ParsedHtmlElement>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedHtmlParagraph {
    pub text_align: Option<TextAlign>,
    /// The paragraph's `font-size`, as a percentage of the surrounding text.
    /// Only a whole paragraph can be resized: GPUI lays a line out at a single
    /// font size, so a `font-size` on part of one is not drawn.
    pub font_scale_percent: Option<u16>,
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
    /// The text color as `0xRRGGBB`.
    pub color: Option<u32>,
    /// The background color as `0xRRGGBB`.
    pub background_color: Option<u32>,
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
    /// A `font-size` inherited from an enclosing container such as
    /// `<div style="font-size: 1.25em">`, applied to the paragraphs inside it.
    font_scale_percent: Option<u16>,
}

impl Default for ParseHtmlNodeContext {
    fn default() -> Self {
        Self {
            list_item_depth: 1,
            text_align: None,
            inherited_styles: Vec::new(),
            font_scale_percent: None,
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

    assign_details_keys(&mut children, &mut HashMap::new());

    let (unclosed_details, unmatched_details_closes) = count_details_tags(source);

    Some(ParsedHtmlBlock {
        source_range,
        children,
        unclosed_details,
        unmatched_details_closes,
        details_span: None,
        container_tags: container_tags(source),
        aligned_spans: Vec::new(),
    })
}

/// Lists the `<div>`, `<center>` and `<details>` tags of a raw HTML block in
/// order, with the alignment each opening tag asks for. html5ever's tree
/// builder silently closes whatever the block left open, so the tags are read
/// with its tokenizer instead, which still skips comments and quoted `>`.
fn container_tags(source: &str) -> Vec<HtmlContainerTag> {
    struct ContainerTagSink(Vec<HtmlContainerTag>);

    impl TokenSink for ContainerTagSink {
        type Handle = ();

        fn process_token(&mut self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
            if let Token::TagToken(tag) = token {
                let name = match tag.name.as_ref() {
                    "div" => Some(HtmlContainerName::Div),
                    "center" => Some(HtmlContainerName::Center),
                    "details" => Some(HtmlContainerName::Details),
                    _ => None,
                };
                if let Some(name) = name {
                    self.0.push(match tag.kind {
                        TagKind::StartTag => {
                            let attrs = RefCell::new(tag.attrs);
                            let styles = extract_styles_from_attributes(&attrs);
                            let text_align = text_align_from_attributes(&attrs, &styles)
                                .or((name == HtmlContainerName::Center)
                                    .then_some(TextAlign::Center));
                            HtmlContainerTag::Open { name, text_align }
                        }
                        TagKind::EndTag => HtmlContainerTag::Close(name),
                    });
                }
            }
            TokenSinkResult::Continue
        }
    }

    let mut tokenizer = Tokenizer::new(ContainerTagSink(Vec::new()), TokenizerOpts::default());
    let mut input = BufferQueue::default();
    input.push_back(StrTendril::from(source));
    // The sink never asks to run a script, so this finishes in one pass; the
    // loop is how the tokenizer expects to be resumed if it ever pauses.
    while let TokenizerResult::Script(()) = tokenizer.feed(&mut input) {}
    tokenizer.end();
    tokenizer.sink.0
}

/// Gives the elements of a block that sits inside an aligned container opened
/// by an earlier block the alignment they would have inherited had the
/// container been written in the same block, where an element's own alignment
/// still wins. Tables are left alone there too.
#[stacksafe]
pub(crate) fn inherit_text_align(elements: &mut [ParsedHtmlElement], text_align: TextAlign) {
    for element in elements {
        match element {
            ParsedHtmlElement::Paragraph(paragraph) => {
                paragraph.text_align.get_or_insert(text_align);
            }
            ParsedHtmlElement::Heading(heading) => {
                heading.text_align.get_or_insert(text_align);
            }
            ParsedHtmlElement::List(list) => {
                for item in &mut list.items {
                    inherit_text_align(&mut item.content, text_align);
                }
            }
            ParsedHtmlElement::BlockQuote(block_quote) => {
                inherit_text_align(&mut block_quote.children, text_align);
            }
            ParsedHtmlElement::Details(details) => {
                inherit_text_align(&mut details.children, text_align);
            }
            ParsedHtmlElement::Image(image) => {
                *element = ParsedHtmlElement::Paragraph(ParsedHtmlParagraph {
                    text_align: Some(text_align),
                    font_scale_percent: None,
                    contents: vec![HtmlParagraphChunk::Image(image.clone())],
                });
            }
            ParsedHtmlElement::Table(_) | ParsedHtmlElement::Rule(_) => {}
        }
    }
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

/// Numbers the disclosures of every block in document order, so two disclosures
/// that share a summary still get keys of their own. Called once the whole
/// document is parsed, since a block cannot know what came before it.
pub(crate) fn assign_details_ordinals(blocks: &mut BTreeMap<usize, ParsedHtmlBlock>) {
    let mut ordinals = HashMap::new();
    for block in blocks.values_mut() {
        assign_details_keys(&mut block.children, &mut ordinals);
    }
}

#[stacksafe]
fn assign_details_keys(elements: &mut [ParsedHtmlElement], ordinals: &mut HashMap<u64, u32>) {
    for element in elements {
        match element {
            ParsedHtmlElement::Details(details) => {
                let summary = html_paragraph_text(&details.summary);
                let key = html_details_key(&summary, 0);
                let ordinal = ordinals.entry(key.summary_hash).or_insert(0);
                details.key = html_details_key(&summary, *ordinal);
                *ordinal += 1;
                assign_details_keys(&mut details.children, ordinals);
            }
            ParsedHtmlElement::BlockQuote(block_quote) => {
                assign_details_keys(&mut block_quote.children, ordinals);
            }
            ParsedHtmlElement::List(list) => {
                for item in &mut list.items {
                    assign_details_keys(&mut item.content, ordinals);
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn html_paragraph_text(paragraph: &HtmlParagraph) -> String {
    let mut text = String::new();
    for chunk in paragraph {
        match chunk {
            HtmlParagraphChunk::Text(chunk) => text.push_str(chunk.contents.as_ref()),
            HtmlParagraphChunk::Image(image) => {
                if let Some(alt_text) = &image.alt_text {
                    text.push_str(alt_text.as_ref());
                }
            }
        }
    }
    text
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
                font_scale_percent: context.font_scale_percent,
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
            let font_scale_percent = styles_map
                .get("font-size")
                .and_then(|value| parse_css_font_scale(value))
                .or(context.font_scale_percent);
            let mut styles = context.inherited_styles.clone();
            if let Some(style) = html_style_from_html_styles(styles_map) {
                styles.push(style);
            }

            if name.local == local_name!("img") {
                if let Some(image) = extract_image(source_range, attrs) {
                    // An image has no alignment of its own, so the one a
                    // `<div align="center">` around it asks for only reaches it
                    // through a paragraph, the way it reaches text in that div.
                    // The image's own `align` means floating, not this.
                    elements.push(match context.text_align {
                        Some(text_align) => ParsedHtmlElement::Paragraph(ParsedHtmlParagraph {
                            text_align: Some(text_align),
                            font_scale_percent: context.font_scale_percent,
                            contents: vec![HtmlParagraphChunk::Image(image)],
                        }),
                        None => ParsedHtmlElement::Image(image),
                    });
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
                        font_scale_percent,
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
                    let mut list = ParsedHtmlElement::List(list);
                    // The items start from a fresh context, so an enclosing
                    // `<div align>` reaches their text the same way it reaches a
                    // list written as markdown after the `<div>`'s blank line.
                    if let Some(text_align) = text_align {
                        inherit_text_align(std::slice::from_mut(&mut list), text_align);
                    }
                    elements.push(list);
                }
            } else if name.local == local_name!("blockquote") {
                if let Some(blockquote) = extract_html_blockquote(node, source_range) {
                    let mut blockquote = ParsedHtmlElement::BlockQuote(blockquote);
                    if let Some(text_align) = text_align {
                        inherit_text_align(std::slice::from_mut(&mut blockquote), text_align);
                    }
                    elements.push(blockquote);
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
                        font_scale_percent,
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
                        font_scale_percent: context.font_scale_percent,
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
                        font_scale_percent,
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

    for chunk in &mut summary {
        if let HtmlParagraphChunk::Text(text) = chunk {
            apply_markdown_inlines(text);
        }
    }

    let mut children = Vec::new();
    consume_nodes(source_range.clone(), &body_nodes, &mut children, context);

    ParsedHtmlDetails {
        source_range,
        key: HtmlDetailsKey::default(),
        open: attr_value_named(attrs, "open").is_some(),
        summary,
        children,
    }
}

/// The inline markdown a `<summary>` is allowed to contain. The surrounding HTML
/// block is opaque to the markdown parser, so `**bold**` and `[a](b)` reach the
/// summary as literal text and have to be parsed here, the way GitHub does.
const SUMMARY_MARKDOWN_OPTIONS: Options = Options::ENABLE_STRIKETHROUGH;

fn apply_markdown_inlines(text: &mut ParsedHtmlText) {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let source = text.contents.clone();
    if !source.contains(['*', '_', '`', '[', '~']) {
        return;
    }

    let mut contents = String::with_capacity(source.len());
    let mut highlights = Vec::new();
    let mut links = Vec::new();
    let mut open_styles: Vec<(HtmlHighlightStyle, usize)> = Vec::new();
    let mut open_links: Vec<(SharedString, usize)> = Vec::new();
    // Where each piece of the source ended up in `contents`, so the highlights
    // the HTML tags already produced can be moved onto the stripped text.
    let mut moved: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    let mut paragraphs = 0;

    let push = |contents: &mut String,
                moved: &mut Vec<(Range<usize>, Range<usize>)>,
                source_range: Range<usize>,
                piece: &str| {
        let start = contents.len();
        contents.push_str(piece);
        moved.push((source_range, start..contents.len()));
    };

    for (event, range) in Parser::new_ext(&source, SUMMARY_MARKDOWN_OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::Paragraph) => {
                paragraphs += 1;
                if paragraphs > 1 {
                    return;
                }
            }
            Event::End(TagEnd::Paragraph) => {}
            Event::Text(parsed) => push(&mut contents, &mut moved, range, &parsed),
            Event::Code(parsed) => {
                let start = contents.len();
                push(&mut contents, &mut moved, range, &parsed);
                highlights.push((
                    start..contents.len(),
                    HtmlHighlightStyle {
                        code: true,
                        ..Default::default()
                    },
                ));
            }
            Event::SoftBreak => push(&mut contents, &mut moved, range, " "),
            Event::HardBreak => push(&mut contents, &mut moved, range, "\n"),
            Event::Start(Tag::Emphasis) => open_styles.push((
                HtmlHighlightStyle {
                    italic: true,
                    ..Default::default()
                },
                contents.len(),
            )),
            Event::Start(Tag::Strong) => open_styles.push((
                HtmlHighlightStyle {
                    weight: FontWeight::BOLD,
                    ..Default::default()
                },
                contents.len(),
            )),
            Event::Start(Tag::Strikethrough) => open_styles.push((
                HtmlHighlightStyle {
                    strikethrough: true,
                    ..Default::default()
                },
                contents.len(),
            )),
            Event::Start(Tag::Link { dest_url, .. }) => {
                open_styles.push((
                    HtmlHighlightStyle {
                        link: true,
                        ..Default::default()
                    },
                    contents.len(),
                ));
                open_links.push((dest_url.into_string().into(), contents.len()));
            }
            Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough) => {
                let Some((style, start)) = open_styles.pop() else {
                    return;
                };
                highlights.push((start..contents.len(), style));
            }
            Event::End(TagEnd::Link) => {
                let Some((style, start)) = open_styles.pop() else {
                    return;
                };
                highlights.push((start..contents.len(), style));
                let Some((url, start)) = open_links.pop() else {
                    return;
                };
                links.push((start..contents.len(), url));
            }
            // Anything else means the summary is not plain inline markdown, so
            // it is left exactly as the HTML parser produced it.
            _ => return,
        }
    }

    if !open_styles.is_empty() || !open_links.is_empty() || contents == source.as_ref() {
        return;
    }

    let offsets = moved_offsets(source.len(), &moved);
    // A piece can be shorter than the source it came from (a code span loses its
    // backticks), so an offset inside it maps to a byte of the rewritten text
    // that may sit inside a character. The renderer slices the text at these
    // offsets, which panics unless they are pulled back to a boundary.
    let move_offset = |offset: usize| {
        let mut moved = offsets.get(offset).copied().unwrap_or(contents.len());
        while moved > 0 && !contents.is_char_boundary(moved) {
            moved -= 1;
        }
        moved
    };
    let move_range = |range: &Range<usize>| move_offset(range.start)..move_offset(range.end);
    highlights.extend(
        text.highlights
            .iter()
            .map(|(range, style)| (move_range(range), style.clone())),
    );
    highlights.retain(|(range, _)| range.start < range.end);
    highlights.sort_by_key(|(range, _)| (range.start, range.end));
    links.extend(
        text.links
            .iter()
            .map(|(range, url)| (move_range(range), url.clone())),
    );
    links.retain(|(range, _)| range.start < range.end);

    text.contents = contents.into();
    text.highlights = highlights;
    text.links = links;
}

/// Maps every offset of the original text onto the rewritten text, so ranges
/// that were measured against the original still cover the same characters.
fn moved_offsets(source_len: usize, moved: &[(Range<usize>, Range<usize>)]) -> Vec<usize> {
    let mut offsets = vec![0; source_len + 1];
    let mut source_end = 0;
    let mut moved_end = 0;

    for (source_range, moved_range) in moved {
        for offset in source_end..source_range.start {
            if let Some(slot) = offsets.get_mut(offset) {
                *slot = moved_end;
            }
        }
        for offset in source_range.start..source_range.end {
            let within = offset.saturating_sub(source_range.start);
            if let Some(slot) = offsets.get_mut(offset) {
                *slot = moved_range.start + within.min(moved_range.len());
            }
        }
        source_end = source_range.end;
        moved_end = moved_range.end;
    }

    for offset in source_end..=source_len {
        if let Some(slot) = offsets.get_mut(offset) {
            *slot = moved_end;
        }
    }

    offsets
}

/// The subscript and superscript characters Unicode actually has. GPUI lays a
/// whole line out at one font size, so a smaller run is not expressible; these
/// are the only way `<sub>` and `<sup>` can come out smaller and off the
/// baseline, and anything outside them is left at full size.
fn unicode_script_text(text: &str, superscript: bool) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    text.chars()
        .map(|character| {
            if superscript {
                match character {
                    '0' => Some('⁰'),
                    '1' => Some('¹'),
                    '2' => Some('²'),
                    '3' => Some('³'),
                    '4' => Some('⁴'),
                    '5' => Some('⁵'),
                    '6' => Some('⁶'),
                    '7' => Some('⁷'),
                    '8' => Some('⁸'),
                    '9' => Some('⁹'),
                    '+' => Some('⁺'),
                    '-' => Some('⁻'),
                    '=' => Some('⁼'),
                    '(' => Some('⁽'),
                    ')' => Some('⁾'),
                    'n' => Some('ⁿ'),
                    'i' => Some('ⁱ'),
                    _ => None,
                }
            } else {
                match character {
                    '0' => Some('₀'),
                    '1' => Some('₁'),
                    '2' => Some('₂'),
                    '3' => Some('₃'),
                    '4' => Some('₄'),
                    '5' => Some('₅'),
                    '6' => Some('₆'),
                    '7' => Some('₇'),
                    '8' => Some('₈'),
                    '9' => Some('₉'),
                    '+' => Some('₊'),
                    '-' => Some('₋'),
                    '=' => Some('₌'),
                    '(' => Some('₍'),
                    ')' => Some('₎'),
                    'a' => Some('ₐ'),
                    'e' => Some('ₑ'),
                    'o' => Some('ₒ'),
                    'x' => Some('ₓ'),
                    'h' => Some('ₕ'),
                    'k' => Some('ₖ'),
                    'l' => Some('ₗ'),
                    'm' => Some('ₘ'),
                    'n' => Some('ₙ'),
                    'p' => Some('ₚ'),
                    's' => Some('ₛ'),
                    't' => Some('ₜ'),
                    _ => None,
                }
            }
        })
        .collect()
}

pub(crate) fn unicode_script_for_tag(text: &str, tag: &str) -> Option<String> {
    match tag {
        "sup" => unicode_script_text(text, true),
        "sub" => unicode_script_text(text, false),
        _ => None,
    }
}

/// The text of an element that holds nothing but text. Swapping the characters
/// of a `<sub>` or `<sup>` throws away whatever the children carried, and the
/// footnote spelling `<sup><a href="#fn-1">1</a></sup>` would lose its link, so
/// an element with children of its own keeps the rendering it already had.
fn text_only_contents(node: &Node) -> Option<String> {
    let mut text = String::new();
    for child in node.children.borrow().iter() {
        let NodeData::Text { contents } = &child.data else {
            return None;
        };
        text.push_str(&contents.borrow().to_string());
    }
    Some(text)
}

#[stacksafe]
fn parse_paragraph(
    source_range: Range<usize>,
    node: &Node,
    paragraph: &mut HtmlParagraph,
    highlights: &mut Vec<HtmlHighlightStyle>,
    links: &mut Vec<SharedString>,
) {
    let consume_styled = |style: HtmlHighlightStyle,
                          paragraph: &mut HtmlParagraph,
                          highlights: &mut Vec<HtmlHighlightStyle>,
                          links: &mut Vec<SharedString>| {
        consume_styled_paragraph(
            source_range.clone(),
            node,
            paragraph,
            style,
            highlights,
            links,
        )
    };

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
                consume_styled(
                    HtmlHighlightStyle {
                        weight: FontWeight::BOLD,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if name.local == local_name!("i") {
                consume_styled(
                    HtmlHighlightStyle {
                        italic: true,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if name.local == local_name!("em") {
                consume_styled(
                    HtmlHighlightStyle {
                        oblique: true,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if name.local == local_name!("del") {
                consume_styled(
                    HtmlHighlightStyle {
                        strikethrough: true,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if name.local == local_name!("ins") || name.local == local_name!("u") {
                consume_styled(
                    HtmlHighlightStyle {
                        underline: true,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if matches!(name.local.as_ref(), "s" | "strike") {
                consume_styled(
                    HtmlHighlightStyle {
                        strikethrough: true,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if matches!(name.local.as_ref(), "code" | "kbd" | "samp" | "tt" | "var") {
                consume_styled(
                    HtmlHighlightStyle {
                        code: true,
                        ..Default::default()
                    },
                    paragraph,
                    highlights,
                    links,
                );
            } else if matches!(name.local.as_ref(), "span" | "mark" | "font") {
                let styles = extract_styles_from_attributes(attrs);
                let font_color = attr_value_named(attrs, "color");
                let (color, background_color) =
                    inline_tag_colors(name.local.as_ref(), &styles, font_color.as_deref());
                let mut style = html_style_from_html_styles(styles).unwrap_or_default();
                style.color = color;
                style.background_color = background_color;
                if style == HtmlHighlightStyle::default() {
                    consume_paragraph(source_range, node, paragraph, highlights, links);
                } else {
                    consume_styled(style, paragraph, highlights, links);
                }
            } else if matches!(name.local.as_ref(), "sub" | "sup") {
                let substituted = text_only_contents(node)
                    .and_then(|contents| unicode_script_for_tag(&contents, name.local.as_ref()));
                match substituted {
                    Some(contents) => {
                        append_paragraph_text(source_range, paragraph, &contents, highlights, links)
                    }
                    None => consume_paragraph(source_range, node, paragraph, highlights, links),
                }
            } else if name.local == local_name!("br") {
                append_paragraph_text(source_range, paragraph, "\n", &[], &[]);
            } else if name.local == local_name!("a") {
                match attr_value(attrs, local_name!("href")) {
                    Some(url) => {
                        let link_depth = links.len();
                        links.push(url.into());
                        consume_styled(
                            HtmlHighlightStyle {
                                link: true,
                                ..Default::default()
                            },
                            paragraph,
                            highlights,
                            links,
                        );
                        links.truncate(link_depth);
                    }
                    None => consume_paragraph(source_range, node, paragraph, highlights, links),
                }
            } else {
                consume_paragraph(source_range, node, paragraph, highlights, links);
            }
        }
        _ => {}
    }
}

/// Parses an element's children with `style` in force, and only for them: a
/// tag's style covers every descendant rather than just the first run of text
/// inside it, so `<span style="color: red">a <b>b</b> c</span>` keeps all three
/// red and the text after the element is left alone.
fn consume_styled_paragraph(
    source_range: Range<usize>,
    node: &Node,
    paragraph: &mut HtmlParagraph,
    style: HtmlHighlightStyle,
    highlights: &mut Vec<HtmlHighlightStyle>,
    links: &mut Vec<SharedString>,
) {
    let depth = highlights.len();
    highlights.push(style);
    consume_paragraph(source_range, node, paragraph, highlights, links);
    highlights.truncate(depth);
}

/// Appends `contents` to the paragraph's trailing text chunk, or starts a new
/// one, attributing the highlights and links in force to the appended range.
/// They are listed outermost first, so an inner element's color is applied
/// after an outer one's and wins.
fn append_paragraph_text(
    source_range: Range<usize>,
    paragraph: &mut HtmlParagraph,
    contents: &str,
    highlights: &[HtmlHighlightStyle],
    links: &[SharedString],
) {
    fn items_with_range<T: Clone>(range: Range<usize>, items: &[T]) -> Vec<(Range<usize>, T)> {
        items
            .iter()
            .map(|item| (range.clone(), item.clone()))
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
            highlights,
        ));
        text.links
            .extend(items_with_range(text.contents.len()..new_text.len(), links));
        text.contents = SharedString::from(new_text);
    } else {
        paragraph.push(HtmlParagraphChunk::Text(ParsedHtmlText {
            source_range,
            highlights: items_with_range(0..contents.len(), highlights),
            links: items_with_range(0..contents.len(), links),
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
                font_scale_percent: context.font_scale_percent,
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

    let (color, background_color) = style_colors(&styles);
    html_style.color = color;
    html_style.background_color = background_color;

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
    attr_value(attrs, local_name!("style"))
        .map(|style| parse_style_declarations(&style))
        .unwrap_or_default()
}

/// The declarations of a `style` attribute, keyed by their lowercased property.
pub(crate) fn parse_style_declarations(style: &str) -> HashMap<String, String> {
    let mut styles = HashMap::new();
    for declaration in style.split(';') {
        let mut parts = declaration.splitn(2, ':');
        if let Some((key, value)) = parts.next().zip(parts.next()) {
            styles.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }
    styles
}

/// The background a browser gives `<mark>` when nothing says otherwise.
pub(crate) const DEFAULT_MARK_BACKGROUND: u32 = 0xffff00;

/// The text color a browser gives `<mark>`. A dark theme's light text would
/// be unreadable on the yellow, so the preview follows the browser here too,
/// for a `<mark>` of any color.
pub(crate) const MARK_TEXT_COLOR: u32 = 0x000000;

/// The colors an inline `<span>`, `<mark>` or `<font>` asks for: its `style`
/// declarations, the `color` attribute of a `<font>`, and the defaults a
/// browser gives `<mark>`.
pub(crate) fn inline_tag_colors(
    tag_name: &str,
    styles: &HashMap<String, String>,
    font_color_attribute: Option<&str>,
) -> (Option<u32>, Option<u32>) {
    let (color, background) = style_colors(styles);
    match tag_name {
        "mark" => (
            color.or(Some(MARK_TEXT_COLOR)),
            background.or(Some(DEFAULT_MARK_BACKGROUND)),
        ),
        "font" => (
            color.or_else(|| font_color_attribute.and_then(parse_css_color)),
            background,
        ),
        _ => (color, background),
    }
}

/// The text and background colors of a set of `style` declarations. The
/// `background` shorthand only counts when it is nothing but a color.
fn style_colors(styles: &HashMap<String, String>) -> (Option<u32>, Option<u32>) {
    let color = styles.get("color").and_then(|value| parse_css_color(value));
    let background = styles
        .get("background-color")
        .or_else(|| styles.get("background"))
        .and_then(|value| parse_css_color(value));
    (color, background)
}

/// A CSS color as `0xRRGGBB`: `#rgb`, `#rrggbb`, `rgb(r, g, b)` or one of the
/// basic color names. Anything else, a color with transparency included, is
/// not understood, and the caller leaves the text in its usual color.
pub(crate) fn parse_css_color(value: &str) -> Option<u32> {
    let value = value.trim().to_ascii_lowercase();

    if let Some(hex) = value.strip_prefix('#') {
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        return match hex.len() {
            6 => u32::from_str_radix(hex, 16).ok(),
            3 => {
                let expanded = hex
                    .chars()
                    .flat_map(|digit| [digit, digit])
                    .collect::<String>();
                u32::from_str_radix(&expanded, 16).ok()
            }
            _ => None,
        };
    }

    if let Some(components) = value
        .strip_prefix("rgb(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let components = components
            .split([',', ' '])
            .filter(|component| !component.is_empty())
            .map(|component| component.parse::<u8>().ok())
            .collect::<Option<Vec<_>>>()?;
        let [red, green, blue] = components.as_slice() else {
            return None;
        };
        return Some(u32::from(*red) << 16 | u32::from(*green) << 8 | u32::from(*blue));
    }

    Some(match value.as_str() {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xff0000,
        "green" => 0x008000,
        "blue" => 0x0000ff,
        "yellow" => 0xffff00,
        "orange" => 0xffa500,
        "purple" => 0x800080,
        "pink" => 0xffc0cb,
        "gray" | "grey" => 0x808080,
        "brown" => 0xa52a2a,
        "navy" => 0x000080,
        "teal" => 0x008080,
        "maroon" => 0x800000,
        "olive" => 0x808000,
        "lime" => 0x00ff00,
        "aqua" | "cyan" => 0x00ffff,
        "fuchsia" | "magenta" => 0xff00ff,
        "silver" => 0xc0c0c0,
        _ => return None,
    })
}

/// The largest `font-size` the preview draws, relative to the surrounding
/// text. A paragraph written at ten times the size would push everything
/// else out of the preview.
const MAX_FONT_SCALE_PERCENT: f32 = 400.0;
const MIN_FONT_SCALE_PERCENT: f32 = 25.0;

/// A CSS `font-size` relative to the surrounding text, as a percentage:
/// `1.25em`, `125%`, or `1.25rem`, which the preview treats like `em` since its
/// body text is close to the root size. Absolute sizes and keywords are not
/// understood.
pub(crate) fn parse_css_font_scale(value: &str) -> Option<u16> {
    let value = value.trim().to_ascii_lowercase();
    let percent = if let Some(number) = value
        .strip_suffix("rem")
        .or_else(|| value.strip_suffix("em"))
    {
        number.trim().parse::<f32>().ok()? * 100.0
    } else if let Some(number) = value.strip_suffix('%') {
        number.trim().parse::<f32>().ok()?
    } else {
        return None;
    };
    if !percent.is_finite() || percent <= 0.0 {
        return None;
    }
    Some(
        percent
            .round()
            .clamp(MIN_FONT_SCALE_PERCENT, MAX_FONT_SCALE_PERCENT) as u16,
    )
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
    fn parses_nested_details_with_distinct_keys() {
        let parsed = parse(
            "<details><summary>外</summary><details><summary>内</summary>中身</details></details>",
        );

        let ParsedHtmlElement::Details(outer) = &parsed.children[0] else {
            panic!("expected outer details");
        };
        assert_eq!(outer.key, html_details_key("外", 0));
        assert_eq!(paragraph_chunks_text(&outer.summary), "外");

        let ParsedHtmlElement::Details(inner) = &outer.children[0] else {
            panic!("expected inner details");
        };
        assert_eq!(inner.key, html_details_key("内", 0));
        assert_ne!(inner.key, outer.key);
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
    fn lists_container_tags_in_order_with_their_alignment() {
        use HtmlContainerName::{Center, Details, Div};

        assert_eq!(
            parse("<div align=\"center\">\n<img src=\"logo.png\">").container_tags,
            vec![HtmlContainerTag::Open {
                name: Div,
                text_align: Some(TextAlign::Center),
            }]
        );
        assert_eq!(
            parse("<DIV ALIGN=RIGHT>").container_tags,
            vec![HtmlContainerTag::Open {
                name: Div,
                text_align: Some(TextAlign::Right),
            }]
        );
        assert_eq!(
            parse("<div align=\"left\" style=\"color: red; text-align: center\">").container_tags,
            vec![HtmlContainerTag::Open {
                name: Div,
                text_align: Some(TextAlign::Center),
            }],
            "the style wins over the attribute, as it does inside one block"
        );
        assert_eq!(
            parse("<center>").container_tags,
            vec![HtmlContainerTag::Open {
                name: Center,
                text_align: Some(TextAlign::Center),
            }]
        );
        assert_eq!(
            parse("</div>\n<details><summary>題</summary>").container_tags,
            vec![
                HtmlContainerTag::Close(Div),
                HtmlContainerTag::Open {
                    name: Details,
                    text_align: None,
                },
            ],
            "a block that closes one container and opens the next keeps the order"
        );
        assert_eq!(
            parse("<div title=\"a > b\"><!-- </div> --><p align=\"center\">x</p></div>")
                .container_tags,
            vec![
                HtmlContainerTag::Open {
                    name: Div,
                    text_align: None,
                },
                HtmlContainerTag::Close(Div),
            ],
            "quoted `>` and comments do not end or add tags, and `<p>` is not followed"
        );
    }

    #[test]
    fn list_and_block_quote_text_inherit_the_container_alignment() {
        let parsed = parse(
            "<div align=\"center\"><ul><li>項目</li></ul><blockquote><p>引用</p></blockquote></div>",
        );

        let ParsedHtmlElement::List(list) = &parsed.children[0] else {
            panic!("expected list, got {:?}", parsed.children);
        };
        let ParsedHtmlElement::Paragraph(item) = &list.items[0].content[0] else {
            panic!("expected the item's text, got {:?}", list.items[0].content);
        };
        assert_eq!(item.text_align, Some(TextAlign::Center));

        let ParsedHtmlElement::BlockQuote(block_quote) = &parsed.children[1] else {
            panic!("expected block quote, got {:?}", parsed.children);
        };
        let ParsedHtmlElement::Paragraph(quoted) = &block_quote.children[0] else {
            panic!("expected the quoted text, got {:?}", block_quote.children);
        };
        assert_eq!(quoted.text_align, Some(TextAlign::Center));

        let parsed = parse("<ul><li>項目</li></ul>");
        let ParsedHtmlElement::List(list) = &parsed.children[0] else {
            panic!("expected list, got {:?}", parsed.children);
        };
        let ParsedHtmlElement::Paragraph(item) = &list.items[0].content[0] else {
            panic!("expected the item's text, got {:?}", list.items[0].content);
        };
        assert_eq!(item.text_align, None, "without a container nothing changes");
    }

    #[test]
    fn inheriting_an_alignment_keeps_the_elements_own() {
        let mut parsed = parse(
            "<p>既定</p><p align=\"left\">左</p><img src=\"a.png\"><details><summary>題</summary><h2>中</h2></details><table><tr><td>x</td></tr></table>",
        );

        inherit_text_align(&mut parsed.children, TextAlign::Right);

        let alignments = parsed
            .children
            .iter()
            .map(|element| match element {
                ParsedHtmlElement::Paragraph(paragraph) => {
                    let is_image = matches!(
                        paragraph.contents.as_slice(),
                        [HtmlParagraphChunk::Image(_)]
                    );
                    (
                        if is_image { "image" } else { "paragraph" },
                        paragraph.text_align,
                    )
                }
                ParsedHtmlElement::Details(details) => match &details.children[0] {
                    ParsedHtmlElement::Heading(heading) => ("details heading", heading.text_align),
                    other => panic!("expected heading, got {other:?}"),
                },
                ParsedHtmlElement::Table(_) => ("table", None),
                other => panic!("unexpected element {other:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            alignments,
            vec![
                ("paragraph", Some(TextAlign::Right)),
                ("paragraph", Some(TextAlign::Left)),
                ("image", Some(TextAlign::Right)),
                ("details heading", Some(TextAlign::Right)),
                ("table", None),
            ]
        );
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
    fn an_image_in_an_aligned_container_carries_the_alignment() {
        let parsed = parse(
            "<div align=\"center\">\n  <img src=\"a.png\" alt=\"写真\" width=\"300\">\n</div>",
        );

        assert_eq!(parsed.children.len(), 1);
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected the image to sit in an aligned paragraph");
        };
        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
        let [HtmlParagraphChunk::Image(image)] = paragraph.contents.as_slice() else {
            panic!("expected the paragraph to hold only the image");
        };
        assert_eq!(image.dest_url.as_ref(), "a.png");
        assert_eq!(image.width, Some(px(300.).into()));

        // Without a container there is nothing to align, and the image stays a
        // block of its own as before.
        let parsed = parse("<img src=\"a.png\" align=\"right\">");
        assert!(matches!(
            parsed.children.as_slice(),
            [ParsedHtmlElement::Image(_)]
        ));
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
        assert_eq!(paragraph_text(&parsed.children[0]), "H\u{2082}O x\u{b2}");
    }

    #[test]
    fn keeps_sub_and_sup_text_unicode_has_no_character_for() {
        let parsed = parse("<p>a<sub>zz</sub></p>");
        assert_eq!(paragraph_text(&parsed.children[0]), "azz");
    }

    #[test]
    fn keeps_a_footnote_reference_inside_a_sup() {
        let parsed = parse("<p>a<sup><a href=\"#fn-1\">1</a></sup></p>");
        let ParsedHtmlElement::Paragraph(paragraph) = &parsed.children[0] else {
            panic!("expected paragraph, got {:?}", parsed.children[0]);
        };
        let HtmlParagraphChunk::Text(text) = &paragraph.contents[0] else {
            panic!("expected a text chunk");
        };

        assert_eq!(text.contents.as_ref(), "a1");
        assert_eq!(
            text.links,
            vec![(1..2, SharedString::from("#fn-1"))],
            "swapping in the Unicode superscript would drop the destination"
        );
    }

    #[test]
    fn parses_markdown_inside_a_summary() {
        let parsed = parse(
            "<details><summary>**\u{592a}\u{5b57}** と `code` と [\u{30ea}\u{30f3}\u{30af}](https://example.com)</summary>\u{672c}\u{6587}</details>",
        );
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };

        let HtmlParagraphChunk::Text(summary) = &details.summary[0] else {
            panic!("expected a text chunk");
        };
        assert_eq!(
            summary.contents.as_ref(),
            "\u{592a}\u{5b57} と code と \u{30ea}\u{30f3}\u{30af}"
        );

        let style_at = |offset: usize| {
            summary
                .highlights
                .iter()
                .find(|(range, _)| range.contains(&offset))
                .map(|(_, style)| style.clone())
                .unwrap_or_default()
        };
        assert_eq!(style_at(0).weight, FontWeight::BOLD);
        let code_offset = summary
            .contents
            .find("code")
            .expect("the code span survives");
        assert!(style_at(code_offset).code);
        let link_offset = summary
            .contents
            .find('\u{30ea}')
            .expect("the link text survives");
        assert!(style_at(link_offset).link);
        assert_eq!(
            summary.links,
            vec![(
                link_offset..summary.contents.len(),
                SharedString::from("https://example.com")
            )]
        );
    }

    #[test]
    fn keeps_a_summary_that_is_not_inline_markdown() {
        let parsed = parse("<details><summary>2 * 3 * 4</summary>\u{672c}\u{6587}</details>");
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };
        assert_eq!(paragraph_chunks_text(&details.summary), "2 * 3 * 4");
    }

    #[test]
    fn keeps_the_html_styling_of_a_summary_that_also_has_markdown() {
        let parsed = parse("<details><summary><b>a</b> **b**</summary>\u{672c}\u{6587}</details>");
        let ParsedHtmlElement::Details(details) = &parsed.children[0] else {
            panic!("expected details");
        };
        let HtmlParagraphChunk::Text(summary) = &details.summary[0] else {
            panic!("expected a text chunk");
        };

        assert_eq!(summary.contents.as_ref(), "a b");
        let bold_ranges = summary
            .highlights
            .iter()
            .filter(|(_, style)| style.weight == FontWeight::BOLD)
            .map(|(range, _)| range.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            bold_ranges,
            vec![0..1, 2..3],
            "both the tag and the markdown should be bold: {:?}",
            summary.highlights
        );
    }

    #[test]
    fn numbers_repeated_summaries_apart() {
        let first = parse("<details><summary>\u{984c}</summary>a</details>");
        let second = parse("<details><summary>\u{984c}</summary>b</details>");
        let mut blocks = BTreeMap::new();
        blocks.insert(0, first);
        blocks.insert(100, second);

        assign_details_ordinals(&mut blocks);

        let key_of = |start: &usize| match &blocks[start].children[0] {
            ParsedHtmlElement::Details(details) => details.key,
            element => panic!("expected details, got {element:?}"),
        };
        assert_eq!(key_of(&0), html_details_key("\u{984c}", 0));
        assert_eq!(key_of(&100), html_details_key("\u{984c}", 1));
        assert_ne!(key_of(&0), key_of(&100));
    }

    #[test]
    fn keys_a_disclosure_by_its_summary_rather_than_its_position() {
        let mut moved = BTreeMap::new();
        moved.insert(40, parse("<details><summary>\u{984c}</summary>a</details>"));
        assign_details_ordinals(&mut moved);

        let ParsedHtmlElement::Details(details) = &moved[&40].children[0] else {
            panic!("expected details");
        };
        assert_eq!(details.key, html_details_key("\u{984c}", 0));
    }

    #[test]
    fn parses_hr_as_a_rule() {
        let parsed = parse("<div>a<hr>b</div>");
        assert!(matches!(parsed.children[1], ParsedHtmlElement::Rule(_)));
    }

    /// The style in force at each character of a text chunk, the way the
    /// renderer merges them: in order, later ones winning.
    fn colors_at(text: &ParsedHtmlText, needle: &str) -> (Option<u32>, Option<u32>, FontWeight) {
        let offset = text
            .contents
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in {:?}", text.contents));
        let mut colors = (None, None, FontWeight::default());
        for (range, style) in &text.highlights {
            if range.contains(&offset) {
                colors.0 = style.color.or(colors.0);
                colors.1 = style.background_color.or(colors.1);
                if style.weight != FontWeight::default() {
                    colors.2 = style.weight;
                }
            }
        }
        colors
    }

    fn first_text(element: &ParsedHtmlElement) -> &ParsedHtmlText {
        let ParsedHtmlElement::Paragraph(paragraph) = element else {
            panic!("expected paragraph, got {element:?}");
        };
        let HtmlParagraphChunk::Text(text) = &paragraph.contents[0] else {
            panic!("expected a text chunk");
        };
        text
    }

    #[test]
    fn parses_colored_inline_tags_in_an_html_block() {
        let parsed = parse(
            "<p>a <span style=\"color: #d44c47\">赤 <b>太</b> 続</span> 後 <mark>黄</mark> \
             <mark style=\"background-color: #b7ebc6\">緑</mark> <font color=\"#00f\">青</font></p>",
        );
        let text = first_text(&parsed.children[0]);

        assert_eq!(colors_at(text, "a"), (None, None, FontWeight::default()));
        assert_eq!(
            colors_at(text, "赤"),
            (Some(0xd44c47), None, FontWeight::default())
        );
        // The color covers everything inside the span, not only its first text.
        assert_eq!(
            colors_at(text, "太"),
            (Some(0xd44c47), None, FontWeight::BOLD)
        );
        assert_eq!(
            colors_at(text, "続"),
            (Some(0xd44c47), None, FontWeight::default())
        );
        assert_eq!(colors_at(text, "後"), (None, None, FontWeight::default()));
        assert_eq!(
            colors_at(text, "黄"),
            (Some(0x000000), Some(0xffff00), FontWeight::default())
        );
        assert_eq!(
            colors_at(text, "緑"),
            (Some(0x000000), Some(0xb7ebc6), FontWeight::default())
        );
        assert_eq!(
            colors_at(text, "青"),
            (Some(0x0000ff), None, FontWeight::default())
        );
    }

    #[test]
    fn inner_colors_win_in_an_html_block() {
        let parsed = parse(
            "<div style=\"color: red\">外 <span style=\"color: blue\">内</span> <span style=\"color: bogus\">無</span></div>",
        );
        let text = first_text(&parsed.children[0]);
        assert_eq!(colors_at(text, "外").0, Some(0xff0000));
        assert_eq!(colors_at(text, "内").0, Some(0x0000ff));
        // A color that cannot be read leaves the one around it in place.
        assert_eq!(colors_at(text, "無").0, Some(0xff0000));
    }

    #[test]
    fn parses_paragraph_font_size_in_an_html_block() {
        let parsed = parse("<p style=\"font-size: 1.25em\">大</p><p>並</p>");
        let sizes = parsed
            .children
            .iter()
            .map(|element| match element {
                ParsedHtmlElement::Paragraph(paragraph) => paragraph.font_scale_percent,
                element => panic!("expected paragraph, got {element:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(sizes, [Some(125), None]);

        // A container passes its size on to the paragraphs inside it.
        let parsed = parse("<div style=\"font-size: 150%\"><p>一</p>二</div><p>三</p>");
        let sizes = parsed
            .children
            .iter()
            .map(|element| match element {
                ParsedHtmlElement::Paragraph(paragraph) => (
                    paragraph_chunks_text(&paragraph.contents),
                    paragraph.font_scale_percent,
                ),
                element => panic!("expected paragraph, got {element:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sizes,
            [("一二".to_string(), Some(150)), ("三".to_string(), None)]
        );
    }

    #[test]
    fn parses_css_colors() {
        assert_eq!(parse_css_color("#d44c47"), Some(0xd44c47));
        assert_eq!(parse_css_color(" #ABC "), Some(0xaabbcc));
        assert_eq!(parse_css_color("rgb(255, 0, 16)"), Some(0xff0010));
        assert_eq!(parse_css_color("rgb(255 0 16)"), Some(0xff0010));
        assert_eq!(parse_css_color("Red"), Some(0xff0000));
        assert_eq!(parse_css_color("grey"), Some(0x808080));
        for unreadable in [
            "",
            "#12",
            "#1234",
            "#gggggg",
            "rgb(256, 0, 0)",
            "rgb(1, 2)",
            "rgba(1, 2, 3, 0.5)",
            "hsl(0, 100%, 50%)",
            "reddish",
        ] {
            assert_eq!(parse_css_color(unreadable), None, "{unreadable:?}");
        }
    }

    #[test]
    fn parses_css_font_scales() {
        assert_eq!(parse_css_font_scale("1.25em"), Some(125));
        assert_eq!(parse_css_font_scale("0.85em"), Some(85));
        assert_eq!(parse_css_font_scale(" 1.5rem "), Some(150));
        assert_eq!(parse_css_font_scale("120%"), Some(120));
        assert_eq!(parse_css_font_scale("20em"), Some(400));
        for unreadable in ["", "16px", "large", "-1em", "0em", "em", "NaNem"] {
            assert_eq!(parse_css_font_scale(unreadable), None, "{unreadable:?}");
        }
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
