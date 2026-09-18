use collections::{BTreeMap, HashMap, HashSet};
use gpui::SharedString;
use linkify::LinkFinder;
pub use pulldown_cmark::TagEnd as MarkdownTagEnd;
pub use pulldown_cmark::{Alignment, HeadingLevel};
use pulldown_cmark::{CowStr, LinkType, MetadataBlockKind, Options, Parser};
use std::{ops::Range, sync::Arc};
use util::markdown::generate_heading_slug;

use crate::{html, path_range::PathWithRange};

pub const PARSE_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_SMART_PUNCTUATION)
    .union(Options::ENABLE_HEADING_ATTRIBUTES)
    .union(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS)
    .union(Options::ENABLE_OLD_FOOTNOTES)
    .union(Options::ENABLE_GFM)
    .union(Options::ENABLE_SUPERSCRIPT)
    .union(Options::ENABLE_SUBSCRIPT);

#[derive(Default)]
struct ParseState {
    events: Vec<(Range<usize>, MarkdownEvent)>,
    root_block_starts: Vec<usize>,
    depth: usize,
}

#[derive(Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedMarkdownData {
    pub events: Vec<(Range<usize>, MarkdownEvent)>,
    pub language_names: HashSet<SharedString>,
    pub language_paths: HashSet<Arc<str>>,
    pub root_block_starts: Vec<usize>,
    pub html_blocks: BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    pub metadata_blocks: BTreeMap<usize, ParsedMetadataBlock>,
    pub heading_slugs: HashMap<SharedString, usize>,
    pub footnote_definitions: HashMap<SharedString, usize>,
    /// Source spans of link reference definitions (`[id]: https://example.com`), which are
    /// consumed by the parser and never appear in any event range.
    pub link_definition_spans: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParsedMetadataBlock {
    pub content_range: Range<usize>,
    pub rows: Option<Vec<MetadataRow>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetadataRow {
    pub key: Range<usize>,
    pub value: Range<usize>,
}

impl ParseState {
    fn push_event(&mut self, range: Range<usize>, event: MarkdownEvent) {
        match &event {
            MarkdownEvent::Start(_) => {
                if self.depth == 0 {
                    self.root_block_starts.push(range.start);
                    self.events.push((range.clone(), MarkdownEvent::RootStart));
                }
                self.depth += 1;
                self.events.push((range, event));
            }
            MarkdownEvent::End(_) => {
                self.events.push((range.clone(), event));
                if self.depth > 0 {
                    self.depth -= 1;
                    if self.depth == 0 {
                        let root_block_index = self.root_block_starts.len() - 1;
                        self.events
                            .push((range, MarkdownEvent::RootEnd(root_block_index)));
                    }
                }
            }
            MarkdownEvent::Rule => {
                if self.depth == 0 && !range.is_empty() {
                    self.root_block_starts.push(range.start);
                    let root_block_index = self.root_block_starts.len() - 1;
                    self.events.push((range.clone(), MarkdownEvent::RootStart));
                    self.events.push((range.clone(), event));
                    self.events
                        .push((range, MarkdownEvent::RootEnd(root_block_index)));
                } else {
                    self.events.push((range, event));
                }
            }
            _ => {
                self.events.push((range, event));
            }
        }
    }
}

const MAX_DUPLICATE_HEADING_SLUGS: usize = 128;

fn build_heading_slugs(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> HashMap<SharedString, usize> {
    let mut slugs = HashMap::default();
    let mut slug_counts: HashMap<String, usize> = HashMap::default();
    let mut inside_heading = false;
    let mut heading_text = String::new();
    let mut heading_source_start: Option<usize> = None;

    for (range, event) in events {
        match event {
            MarkdownEvent::Start(MarkdownTag::Heading { .. }) => {
                inside_heading = true;
                heading_text.clear();
                heading_source_start = None;
            }
            MarkdownEvent::End(MarkdownTagEnd::Heading(_)) => {
                if inside_heading {
                    let source_offset = heading_source_start.unwrap_or(range.start);
                    let base_slug = generate_heading_slug(&heading_text);
                    let count = slug_counts.entry(base_slug.clone()).or_insert(0);
                    let mut slug = if *count == 0 {
                        base_slug.clone()
                    } else {
                        format!("{base_slug}-{count}")
                    };
                    *count += 1;
                    while slugs.contains_key(slug.as_str()) {
                        let Some(count) = slug_counts.get_mut(&base_slug) else {
                            slug.clear();
                            break;
                        };
                        if *count >= MAX_DUPLICATE_HEADING_SLUGS {
                            slug.clear();
                            break;
                        }
                        slug = format!("{base_slug}-{count}");
                        *count += 1;
                    }
                    if !slug.is_empty() {
                        slugs.insert(SharedString::from(slug), source_offset);
                    }
                    inside_heading = false;
                }
            }
            MarkdownEvent::Text | MarkdownEvent::Code if inside_heading => {
                if heading_source_start.is_none() {
                    heading_source_start = Some(range.start);
                }
                heading_text.push_str(&source[range.clone()]);
            }
            MarkdownEvent::SubstitutedCode(substituted) if inside_heading => {
                if heading_source_start.is_none() {
                    heading_source_start = Some(range.start);
                }
                heading_text.push_str(substituted);
            }
            MarkdownEvent::SubstitutedText(substituted) if inside_heading => {
                if heading_source_start.is_none() {
                    heading_source_start = Some(range.start);
                }
                heading_text.push_str(substituted);
            }
            _ => {}
        }
    }

    slugs
}

fn parse_metadata_table_rows(source: &str, source_range: Range<usize>) -> Option<Vec<MetadataRow>> {
    let mut rows = Vec::new();
    let mut line_start = source_range.start;

    for line in source[source_range].split_inclusive('\n') {
        let line_end = line_start + line.len();
        let content_end = line_start + line.trim_end_matches(['\r', '\n']).len();
        let content_range = line_start..content_end;
        let line_text = &source[content_range.clone()];

        if line_text.is_empty()
            || line_text
                .chars()
                .next()
                .is_some_and(|character| character.is_whitespace())
        {
            return None;
        }

        let delimiter = line_text.find(':')?;
        let key = trim_metadata_range(source, content_range.start..content_range.start + delimiter);
        let value = trim_metadata_range(
            source,
            content_range.start + delimiter + 1..content_range.end,
        );
        if key.is_empty() || value.is_empty() {
            return None;
        }

        rows.push(MetadataRow { key, value });
        line_start = line_end;
    }

    if rows.is_empty() { None } else { Some(rows) }
}

fn trim_metadata_range(source: &str, range: Range<usize>) -> Range<usize> {
    let text = &source[range.clone()];
    let start_offset = text.len() - text.trim_start().len();
    let end_offset = text.trim_end().len();
    let start = range.start + start_offset;
    let end = (range.start + end_offset).max(start);
    start..end
}

fn is_br_tag(html: &str) -> bool {
    let Some(inner) = html
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
    else {
        return false;
    };
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    inner
        .split_ascii_whitespace()
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("br"))
}

/// Formatting tags that contribute no text of their own, such as `<kbd>` or
/// `<sup>`. The text-merging path already drops them once a paragraph has
/// started, so matching that here keeps a tag that opens a paragraph from
/// showing up as literal markup. `<code>` is left out because the renderer
/// turns it into an inline code span.
fn is_formatting_only_tag(html: &str) -> bool {
    let Some(inner) = html
        .trim()
        .strip_prefix('<')
        .and_then(|tag| tag.strip_suffix('>'))
    else {
        return false;
    };
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    let inner = inner.strip_prefix('/').unwrap_or(inner);

    inner.split_ascii_whitespace().next().is_some_and(|name| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "abbr"
                | "b"
                | "big"
                | "cite"
                | "del"
                | "em"
                | "font"
                | "i"
                | "ins"
                | "kbd"
                | "mark"
                | "s"
                | "samp"
                | "small"
                | "span"
                | "strike"
                | "strong"
                | "sub"
                | "sup"
                | "tt"
                | "u"
                | "var"
        )
    })
}

/// Parses `text` into the same event stream the preview renders, for consumers
/// that need the events without any GPUI state.
///
/// HTML blocks are left as raw [`MarkdownEvent::Html`] spans rather than being
/// parsed into elements, and metadata blocks are kept so callers can skip them
/// (without [`Options::ENABLE_YAML_STYLE_METADATA_BLOCKS`] a `---` front matter
/// would parse as a rule followed by a setext heading).
///
/// Used by the Markdown → PDF export (DESIGN.md §5.3, ADR-F05).
pub fn parse_markdown_events(text: &str) -> Vec<(Range<usize>, MarkdownEvent)> {
    parse_markdown_with_options(text, false, false, true).events
}

pub(crate) fn parse_markdown_with_options(
    text: &str,
    parse_html: bool,
    parse_heading_slugs: bool,
    parse_metadata_blocks: bool,
) -> ParsedMarkdownData {
    let mut state = ParseState::default();
    let mut language_names = HashSet::default();
    let mut language_paths = HashSet::default();
    let mut html_blocks = BTreeMap::default();
    let mut metadata_blocks = BTreeMap::default();
    let mut within_link = false;
    let mut within_code_block = false;
    let mut within_metadata = false;
    let mut within_table = false;
    let mut current_metadata_block_start = None;
    let mut metadata_block_content_range: Option<Range<usize>> = None;
    let parse_options = if parse_metadata_blocks {
        PARSE_OPTIONS.union(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS)
    } else {
        PARSE_OPTIONS
    };
    let parser = Parser::new_ext(text, parse_options);
    let mut link_definition_spans = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone())
        .collect::<Vec<_>>();
    link_definition_spans.sort_by_key(|span| span.start);
    let mut parser = parser.into_offset_iter().peekable();
    while let Some((pulldown_event, range)) = parser.next() {
        if within_metadata && !parse_metadata_blocks {
            if let pulldown_cmark::Event::End(pulldown_cmark::TagEnd::MetadataBlock(_)) =
                pulldown_event
            {
                within_metadata = false;
                current_metadata_block_start = None;
                metadata_block_content_range = None;
            }
            continue;
        }
        match pulldown_event {
            pulldown_cmark::Event::Start(tag) => {
                if let pulldown_cmark::Tag::HtmlBlock = &tag {
                    state.push_event(range.clone(), MarkdownEvent::Start(MarkdownTag::HtmlBlock));

                    if parse_html {
                        if let Some(block) =
                            html::html_parser::parse_html_block(&text[range.clone()], range.clone())
                        {
                            html_blocks.insert(range.start, block);

                            while let Some((event, end_range)) = parser.next() {
                                if let pulldown_cmark::Event::End(
                                    pulldown_cmark::TagEnd::HtmlBlock,
                                ) = event
                                {
                                    state.push_event(
                                        end_range,
                                        MarkdownEvent::End(MarkdownTagEnd::HtmlBlock),
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    continue;
                }

                let tag = match tag {
                    pulldown_cmark::Tag::Link {
                        link_type,
                        dest_url,
                        title,
                        id,
                    } => {
                        within_link = true;
                        MarkdownTag::Link {
                            link_type,
                            dest_url: SharedString::from(dest_url.into_string()),
                            title: SharedString::from(title.into_string()),
                            id: SharedString::from(id.into_string()),
                        }
                    }
                    pulldown_cmark::Tag::MetadataBlock(kind) => {
                        within_metadata = true;
                        current_metadata_block_start = Some(range.start);
                        metadata_block_content_range = None;
                        if !parse_metadata_blocks {
                            continue;
                        }
                        MarkdownTag::MetadataBlock(kind)
                    }
                    pulldown_cmark::Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Indented) => {
                        within_code_block = true;
                        MarkdownTag::CodeBlock {
                            kind: CodeBlockKind::Indented,
                            metadata: CodeBlockMetadata {
                                content_range: range.clone(),
                                line_count: 1,
                                is_fenced_closed: false,
                            },
                        }
                    }
                    pulldown_cmark::Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(
                        ref info,
                    )) => {
                        within_code_block = true;
                        let code_block_source = &text[range.clone()];
                        let content_range = extract_code_block_content_range(code_block_source);
                        let is_fenced_closed = content_range.end < code_block_source.len();
                        let content_range =
                            content_range.start + range.start..content_range.end + range.start;

                        // Valid to use bytes since multi-byte UTF-8 doesn't use ASCII chars.
                        let line_count = text[content_range.clone()]
                            .bytes()
                            .filter(|c| *c == b'\n')
                            .count();

                        let metadata = CodeBlockMetadata {
                            content_range,
                            line_count,
                            is_fenced_closed,
                        };

                        let info = info.trim();
                        let kind = if info.is_empty() {
                            CodeBlockKind::Fenced
                            // Languages should never contain a slash, and PathRanges always should.
                            // (Models are told to specify them relative to a workspace root.)
                        } else if info.contains('/') {
                            let path_range = PathWithRange::new(info);
                            language_paths.insert(path_range.path.clone());
                            CodeBlockKind::FencedSrc(path_range)
                        } else {
                            let language = SharedString::from(info.to_string());
                            language_names.insert(language.clone());
                            CodeBlockKind::FencedLang(language)
                        };

                        MarkdownTag::CodeBlock { kind, metadata }
                    }
                    pulldown_cmark::Tag::Paragraph => MarkdownTag::Paragraph,
                    pulldown_cmark::Tag::Heading {
                        level,
                        id,
                        classes,
                        attrs,
                    } => {
                        let id = id.map(|id| SharedString::from(id.into_string()));
                        let classes = classes
                            .into_iter()
                            .map(|c| SharedString::from(c.into_string()))
                            .collect();
                        let attrs = attrs
                            .into_iter()
                            .map(|(key, value)| {
                                (
                                    SharedString::from(key.into_string()),
                                    value.map(|v| SharedString::from(v.into_string())),
                                )
                            })
                            .collect();
                        MarkdownTag::Heading {
                            level,
                            id,
                            classes,
                            attrs,
                        }
                    }
                    pulldown_cmark::Tag::BlockQuote(kind) => MarkdownTag::BlockQuote(kind),
                    pulldown_cmark::Tag::List(start_number) => MarkdownTag::List(start_number),
                    pulldown_cmark::Tag::Item => MarkdownTag::Item,
                    pulldown_cmark::Tag::FootnoteDefinition(label) => {
                        MarkdownTag::FootnoteDefinition(SharedString::from(label.to_string()))
                    }
                    pulldown_cmark::Tag::Table(alignments) => {
                        within_table = true;
                        MarkdownTag::Table(alignments)
                    }
                    pulldown_cmark::Tag::TableHead => MarkdownTag::TableHead,
                    pulldown_cmark::Tag::TableRow => MarkdownTag::TableRow,
                    pulldown_cmark::Tag::TableCell => MarkdownTag::TableCell,
                    pulldown_cmark::Tag::Emphasis => MarkdownTag::Emphasis,
                    pulldown_cmark::Tag::Strong => MarkdownTag::Strong,
                    pulldown_cmark::Tag::Strikethrough => MarkdownTag::Strikethrough,
                    pulldown_cmark::Tag::Superscript => MarkdownTag::Superscript,
                    pulldown_cmark::Tag::Subscript => MarkdownTag::Subscript,
                    pulldown_cmark::Tag::Image {
                        link_type,
                        dest_url,
                        title,
                        id,
                    } => MarkdownTag::Image {
                        link_type,
                        dest_url: SharedString::from(dest_url.into_string()),
                        title: SharedString::from(title.into_string()),
                        id: SharedString::from(id.into_string()),
                    },
                    pulldown_cmark::Tag::HtmlBlock => MarkdownTag::HtmlBlock, // this is handled above separately
                    pulldown_cmark::Tag::DefinitionList => MarkdownTag::DefinitionList,
                    pulldown_cmark::Tag::DefinitionListTitle => MarkdownTag::DefinitionListTitle,
                    pulldown_cmark::Tag::DefinitionListDefinition => {
                        MarkdownTag::DefinitionListDefinition
                    }
                };
                state.push_event(range, MarkdownEvent::Start(tag))
            }
            pulldown_cmark::Event::End(tag) => {
                if let pulldown_cmark::TagEnd::Link = tag {
                    within_link = false;
                } else if let pulldown_cmark::TagEnd::CodeBlock = tag {
                    within_code_block = false;
                } else if let pulldown_cmark::TagEnd::MetadataBlock(_) = tag {
                    within_metadata = false;
                    let block_start = current_metadata_block_start.take();
                    let content_range = metadata_block_content_range.take();
                    if parse_metadata_blocks
                        && let (Some(block_start), Some(content_range)) =
                            (block_start, content_range)
                    {
                        metadata_blocks.insert(
                            block_start,
                            ParsedMetadataBlock {
                                rows: parse_metadata_table_rows(text, content_range.clone()),
                                content_range,
                            },
                        );
                    }
                    if !parse_metadata_blocks {
                        continue;
                    }
                } else if let pulldown_cmark::TagEnd::Table = tag {
                    within_table = false;
                }
                state.push_event(range, MarkdownEvent::End(tag));
            }
            pulldown_cmark::Event::Text(parsed) => {
                fn event_for(
                    text: &str,
                    range: Range<usize>,
                    str: &str,
                ) -> (Range<usize>, MarkdownEvent) {
                    if str == &text[range.clone()] {
                        (range, MarkdownEvent::Text)
                    } else {
                        (range, MarkdownEvent::SubstitutedText(str.to_owned()))
                    }
                }

                if within_metadata {
                    match &mut metadata_block_content_range {
                        Some(content_range) => {
                            content_range.start = content_range.start.min(range.start);
                            content_range.end = content_range.end.max(range.end);
                        }
                        None => metadata_block_content_range = Some(range.clone()),
                    }
                    state.push_event(range, MarkdownEvent::Text);
                    continue;
                }

                if within_code_block {
                    let (range, event) = event_for(text, range, &parsed);
                    state.push_event(range, event);
                    continue;
                }

                #[derive(Debug)]
                struct TextRange<'a> {
                    source_range: Range<usize>,
                    merged_range: Range<usize>,
                    parsed: CowStr<'a>,
                }

                let mut last_len = parsed.len();
                let mut ranges = vec![TextRange {
                    source_range: range.clone(),
                    merged_range: 0..last_len,
                    parsed,
                }];

                while match parser.peek() {
                    Some((pulldown_cmark::Event::Text(_), _)) => true,
                    Some((pulldown_cmark::Event::InlineHtml(html), _)) => {
                        parse_html && !is_br_tag(html)
                    }
                    _ => false,
                } {
                    let Some((next_event, next_range)) = parser.next() else {
                        unreachable!()
                    };
                    let next_text = match next_event {
                        pulldown_cmark::Event::Text(next_event) => next_event,
                        pulldown_cmark::Event::InlineHtml(_) => CowStr::Borrowed(""),
                        _ => unreachable!(),
                    };
                    let next_len = last_len + next_text.len();
                    ranges.push(TextRange {
                        source_range: next_range.clone(),
                        merged_range: last_len..next_len,
                        parsed: next_text,
                    });
                    last_len = next_len;
                }

                let mut merged_text =
                    String::with_capacity(ranges.last().unwrap().merged_range.end);
                for range in &ranges {
                    merged_text.push_str(&range.parsed);
                }

                let mut ranges = ranges.into_iter().peekable();

                if !within_link && !within_code_block {
                    let mut finder = LinkFinder::new();
                    finder.kinds(&[linkify::LinkKind::Url]);

                    // Find links in the merged text
                    for link in finder.links(&merged_text) {
                        let link_start_in_merged = link.start();
                        let link_end_in_merged = link.end();

                        while ranges
                            .peek()
                            .is_some_and(|range| range.merged_range.end <= link_start_in_merged)
                        {
                            let range = ranges.next().unwrap();
                            let (range, event) = event_for(text, range.source_range, &range.parsed);
                            state.push_event(range, event);
                        }

                        let Some(range) = ranges.peek_mut() else {
                            continue;
                        };
                        let prefix_len = link_start_in_merged - range.merged_range.start;
                        if prefix_len > 0 {
                            let (head, tail) = range.parsed.split_at(prefix_len);
                            let (event_range, event) = event_for(
                                text,
                                range.source_range.start..range.source_range.start + prefix_len,
                                head,
                            );
                            state.push_event(event_range, event);
                            range.parsed = CowStr::Boxed(tail.into());
                            range.merged_range.start += prefix_len;
                            range.source_range.start += prefix_len;
                        }

                        let link_start_in_source = range.source_range.start;
                        let mut link_end_in_source = range.source_range.end;
                        let mut link_events = Vec::new();

                        while ranges
                            .peek()
                            .is_some_and(|range| range.merged_range.end <= link_end_in_merged)
                        {
                            let range = ranges.next().unwrap();
                            link_end_in_source = range.source_range.end;
                            link_events.push(event_for(text, range.source_range, &range.parsed));
                        }

                        if let Some(range) = ranges.peek_mut() {
                            let prefix_len = link_end_in_merged - range.merged_range.start;
                            if prefix_len > 0 {
                                let (head, tail) = range.parsed.split_at(prefix_len);
                                link_events.push(event_for(
                                    text,
                                    range.source_range.start..range.source_range.start + prefix_len,
                                    head,
                                ));
                                range.parsed = CowStr::Boxed(tail.into());
                                range.merged_range.start += prefix_len;
                                range.source_range.start += prefix_len;
                                link_end_in_source = range.source_range.start;
                            }
                        }
                        let link_range = link_start_in_source..link_end_in_source;

                        state.push_event(
                            link_range.clone(),
                            MarkdownEvent::Start(MarkdownTag::Link {
                                link_type: LinkType::Autolink,
                                dest_url: SharedString::from(link.as_str().to_string()),
                                title: SharedString::default(),
                                id: SharedString::default(),
                            }),
                        );
                        for (range, event) in link_events {
                            state.push_event(range, event);
                        }
                        state.push_event(
                            link_range.clone(),
                            MarkdownEvent::End(MarkdownTagEnd::Link),
                        );
                    }
                }

                for range in ranges {
                    let (range, event) = event_for(text, range.source_range, &range.parsed);
                    state.push_event(range, event);
                }
            }
            pulldown_cmark::Event::Code(parsed) => {
                let content_range = extract_code_content_range(&text[range.clone()]);
                let content_range =
                    content_range.start + range.start..content_range.end + range.start;
                let source = &text[content_range.clone()];
                let event = if within_table && source.contains(r"\|") {
                    MarkdownEvent::SubstitutedCode(parsed.to_string())
                } else {
                    MarkdownEvent::Code
                };
                state.push_event(content_range, event)
            }
            pulldown_cmark::Event::Html(_) => state.push_event(range, MarkdownEvent::Html),
            pulldown_cmark::Event::InlineHtml(html) => {
                if parse_html && is_br_tag(&html) {
                    state.push_event(range, MarkdownEvent::HardBreak)
                } else if parse_html && is_formatting_only_tag(&html) {
                    state.push_event(range, MarkdownEvent::SubstitutedText(String::new()))
                } else {
                    state.push_event(range, MarkdownEvent::InlineHtml)
                }
            }
            pulldown_cmark::Event::FootnoteReference(label) => state.push_event(
                range,
                MarkdownEvent::FootnoteReference(SharedString::from(label.to_string())),
            ),
            pulldown_cmark::Event::SoftBreak => state.push_event(range, MarkdownEvent::SoftBreak),
            pulldown_cmark::Event::HardBreak => state.push_event(range, MarkdownEvent::HardBreak),
            pulldown_cmark::Event::Rule => state.push_event(range, MarkdownEvent::Rule),
            pulldown_cmark::Event::TaskListMarker(checked) => {
                state.push_event(range, MarkdownEvent::TaskListMarker(checked))
            }
            pulldown_cmark::Event::InlineMath(_) | pulldown_cmark::Event::DisplayMath(_) => {}
        }
    }

    if parse_html {
        style_inline_html_tags(&mut state.events, text);
        html::html_parser::assign_details_ordinals(&mut html_blocks);
        link_html_details_spans(&mut html_blocks, &state.events);
    }

    let heading_slugs = if parse_heading_slugs {
        build_heading_slugs(text, &state.events)
    } else {
        HashMap::default()
    };
    let footnote_definitions = build_footnote_definitions(&state.events);

    ParsedMarkdownData {
        events: state.events,
        language_names,
        language_paths,
        root_block_starts: state.root_block_starts,
        html_blocks,
        metadata_blocks,
        heading_slugs,
        footnote_definitions,
        link_definition_spans,
    }
}

/// Pairs each HTML block that leaves a `<details>` open with the later block
/// that closes it. Blank lines inside a disclosure end the CommonMark HTML
/// block, so the recommended GitHub spelling arrives as an opening block, the
/// body as ordinary markdown, and `</details>` as a block of its own.
fn link_html_details_spans(
    html_blocks: &mut BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    events: &[(Range<usize>, MarkdownEvent)],
) {
    let mut open_block_starts = Vec::new();
    let mut spans = Vec::new();

    for (block_start, block) in html_blocks.iter() {
        for _ in 0..block.unmatched_details_closes {
            let Some(open_start) = open_block_starts.pop() else {
                break;
            };
            spans.push((open_start, *block_start));
        }
        for _ in 0..block.unclosed_details {
            open_block_starts.push(*block_start);
        }
    }

    // A block that leaves several disclosures open is recorded once per close it
    // is paired with; the last one wins, which is the outermost disclosure's own
    // `</details>` and so the furthest the renderer may have to skip.
    for (open_start, close_start) in spans {
        let Some(hidden_events) = html_details_body_events(events, open_start, close_start) else {
            continue;
        };

        if let Some(block) = html_blocks.get_mut(&open_start) {
            block.details_span = Some(html::html_parser::HtmlDetailsSpan { hidden_events });
        }
    }
}

/// The events between an opening `<details>` block and the block that closes it,
/// trimmed to the last point where the nesting is back where it started. A
/// disclosure inside a list item or a block quote shares its container with the
/// text around it, so the body cannot be cut at a root block boundary; skipping
/// an unbalanced run would instead leave the renderer's div stack short of a
/// `pop` and misplace everything after it.
fn html_details_body_events(
    events: &[(Range<usize>, MarkdownEvent)],
    open_block_start: usize,
    close_block_start: usize,
) -> Option<Range<usize>> {
    let open_end = events.iter().position(|(range, event)| {
        range.start == open_block_start
            && matches!(event, MarkdownEvent::End(MarkdownTagEnd::HtmlBlock))
    })?;
    let mut start = open_end + 1;
    // The opening block's own root block has to be popped, so the body starts
    // after it rather than at the block's end.
    if matches!(events.get(start), Some((_, MarkdownEvent::RootEnd(_)))) {
        start += 1;
    }

    let mut index = start;
    let mut balanced_end = start;
    let mut depth = 0usize;
    let mut root_depth = 0usize;

    while let Some((range, event)) = events.get(index) {
        if depth == 0 && root_depth == 0 && range.start >= close_block_start {
            break;
        }
        match event {
            MarkdownEvent::Start(_) => depth += 1,
            MarkdownEvent::End(_) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            MarkdownEvent::RootStart => root_depth += 1,
            MarkdownEvent::RootEnd(_) => {
                if root_depth == 0 {
                    break;
                }
                root_depth -= 1;
            }
            _ => {}
        }
        index += 1;
        if depth == 0 && root_depth == 0 {
            balanced_end = index;
        }
    }

    (start < balanced_end).then_some(start..balanced_end)
}

/// The inline HTML tags a plain paragraph may carry that the preview can style
/// instead of dropping. `<b>text</b>` reaches the renderer as two empty
/// substitutions around the text, which used to leave the text unstyled.
///
/// An opening `<a>` also hands back the destination it links to, since that is
/// the one tag whose rewritten events need more than its name.
fn inline_html_style_tag(html: &str) -> Option<(&'static str, Option<String>, bool)> {
    let inner = html
        .trim()
        .strip_prefix('<')
        .and_then(|tag| tag.strip_suffix('>'))?;
    if inner.ends_with('/') {
        return None;
    }
    let (inner, closing) = match inner.strip_prefix('/') {
        Some(inner) => (inner, true),
        None => (inner, false),
    };

    let name = inner.split_ascii_whitespace().next()?.to_ascii_lowercase();
    let name = match name.as_str() {
        "b" | "strong" => "strong",
        "i" | "em" => "em",
        "s" | "del" | "strike" => "del",
        "u" | "ins" => "u",
        "code" | "kbd" | "samp" | "tt" | "var" => "code",
        "sub" => "sub",
        "sup" => "sup",
        "a" => "a",
        _ => return None,
    };

    let destination = if name == "a" && !closing {
        let destination = inline_html_attribute(inner, "href")?;
        // A link with nothing to open, or one that would run a script instead
        // of opening anything, keeps being dropped the way it always was.
        if !is_openable_link_destination(&destination) {
            return None;
        }
        // Whatever opens the destination reads a path, so the surrounding
        // whitespace of `href=" x "` has to go before it is stored.
        Some(destination.trim().to_string())
    } else {
        None
    };

    Some((name, destination, closing))
}

/// The value of the `name` attribute of an opening tag, `inner` being the text
/// between its `<` and `>`. Only the spellings HTML written by hand uses are
/// understood; anything else hands back `None` so the tag is left alone.
fn inline_html_attribute(inner: &str, name: &str) -> Option<String> {
    let tag_name_end = inner.find(char::is_whitespace)?;
    let mut rest = inner.get(tag_name_end..)?;

    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }

        let attribute_name_end = rest
            .find(|character: char| character == '=' || character.is_whitespace())
            .unwrap_or(rest.len());
        let attribute_name = rest.get(..attribute_name_end)?;
        rest = rest.get(attribute_name_end..)?.trim_start();

        let mut value = None;
        if let Some(after_equals) = rest.strip_prefix('=') {
            let after_equals = after_equals.trim_start();
            let (attribute_value, remainder) = match after_equals.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let after_quote = after_equals.get(quote.len_utf8()..)?;
                    let closing_quote = after_quote.find(quote)?;
                    (
                        after_quote.get(..closing_quote)?,
                        after_quote.get(closing_quote + quote.len_utf8()..)?,
                    )
                }
                Some(_) => {
                    let value_end = after_equals
                        .find(char::is_whitespace)
                        .unwrap_or(after_equals.len());
                    (
                        after_equals.get(..value_end)?,
                        after_equals.get(value_end..)?,
                    )
                }
                None => return None,
            };
            value = Some(attribute_value);
            rest = remainder;
        }

        if attribute_name.eq_ignore_ascii_case(name) {
            return value.map(decode_html_entities);
        }
    }
}

/// The longest entity name this understands, so a bare `&` in a URL does not
/// send the scan to the end of the text looking for a `;`.
const MAX_HTML_ENTITY_LENGTH: usize = 10;

/// Expands the HTML entities an attribute is written with. An entity this does
/// not know is left exactly as it was written rather than guessed at.
fn decode_html_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(ampersand) = rest.find('&') {
        let (Some(before), Some(after)) = (rest.get(..ampersand), rest.get(ampersand + 1..)) else {
            break;
        };
        decoded.push_str(before);

        let entity = after
            .find(';')
            .filter(|semicolon| *semicolon <= MAX_HTML_ENTITY_LENGTH)
            .and_then(|semicolon| Some((after.get(..semicolon)?, after.get(semicolon + 1..)?)));
        let Some((entity, remainder)) = entity else {
            decoded.push('&');
            rest = after;
            continue;
        };

        let character = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix(['x', 'X']) {
                    Some(hexadecimal) => u32::from_str_radix(hexadecimal, 16).ok(),
                    None => number.parse().ok(),
                })
                .and_then(char::from_u32),
        };

        match character {
            Some(character) => {
                decoded.push(character);
                rest = remainder;
            }
            None => {
                decoded.push('&');
                rest = after;
            }
        }
    }

    decoded.push_str(rest);
    decoded
}

/// The schemes a click must never hand to the system opener: they run what the
/// document wrote instead of opening something. Checked twice — here, when a
/// link is built out of inline HTML, and again in the preview when one is
/// clicked, since a Markdown link carries whatever destination it was written
/// with.
pub const SCHEMES_THAT_RUN_CODE: &[&str] = &["javascript", "data", "vbscript"];

/// Whether a destination is one the preview may turn into a link. A scheme that
/// runs code rather than opening something would fire on a plain click, so an
/// `<a>` carrying one keeps being dropped instead of becoming a link.
fn is_openable_link_destination(destination: &str) -> bool {
    if destination
        .chars()
        .all(|character| character.is_whitespace() || character.is_control())
    {
        return false;
    }

    match link_destination_scheme(destination) {
        Some(scheme) => !SCHEMES_THAT_RUN_CODE.contains(&scheme.as_str()),
        None => true,
    }
}

/// The scheme whatever opens the destination would act on, or `None` when the
/// destination has none. Everything before the first `:` is a scheme only while
/// it stays within the first path-less segment: `a:b` is a scheme, `./a:b` and
/// `a/b:c` are relative paths.
///
/// The spellings that hide a scheme from a plain `find(':')` are undone first,
/// because what finally opens the destination undoes them too:
/// `open_preview_url` percent-decodes before it looks at the URL, and
/// `Url::parse` drops tabs and newlines before it reads the scheme.
pub fn link_destination_scheme(destination: &str) -> Option<String> {
    let mut scheme = String::new();
    let mut rest = destination;

    loop {
        let mut characters = rest.chars();
        let character = characters.next()?;
        rest = characters.as_str();

        let character = if character == '%' {
            match rest
                .get(..2)
                .filter(|digits| digits.bytes().all(|digit| digit.is_ascii_hexdigit()))
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
            {
                Some(decoded) => {
                    rest = rest.get(2..)?;
                    char::from(decoded)
                }
                None => '%',
            }
        } else {
            character
        };

        match character {
            ':' => return Some(scheme.to_ascii_lowercase()),
            '/' | '?' | '#' => return None,
            _ if character.is_whitespace() || character.is_control() => {}
            _ => scheme.push(character),
        }
    }
}

fn inline_html_style_events(name: &str) -> Option<(MarkdownEvent, MarkdownEvent)> {
    Some(match name {
        "strong" => (
            MarkdownEvent::Start(MarkdownTag::Strong),
            MarkdownEvent::End(MarkdownTagEnd::Strong),
        ),
        "em" => (
            MarkdownEvent::Start(MarkdownTag::Emphasis),
            MarkdownEvent::End(MarkdownTagEnd::Emphasis),
        ),
        "del" => (
            MarkdownEvent::Start(MarkdownTag::Strikethrough),
            MarkdownEvent::End(MarkdownTagEnd::Strikethrough),
        ),
        "u" => (
            MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Underline),
            MarkdownEvent::InlineHtmlStyleEnd(InlineHtmlStyle::Underline),
        ),
        "code" => (
            MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Code),
            MarkdownEvent::InlineHtmlStyleEnd(InlineHtmlStyle::Code),
        ),
        _ => return None,
    })
}

/// Styles the inline formatting tags of ordinary paragraphs, which the HTML
/// block path has been styling since the disclosures landed. Only properly
/// nested pairs are rewritten: an unmatched tag keeps being dropped, so the
/// events stay balanced and the renderer's style stack cannot drift.
fn style_inline_html_tags(events: &mut [(Range<usize>, MarkdownEvent)], text: &str) {
    struct OpenInlineTag {
        name: &'static str,
        /// The destination of an `<a>`; every other tag carries `None`.
        destination: Option<String>,
        index: usize,
        depth: usize,
    }

    let mut open: Vec<OpenInlineTag> = Vec::new();
    let mut pairs = Vec::new();
    let mut depth = 0usize;
    let mut link_depth = 0usize;

    for index in 0..events.len() {
        let Some((range, event)) = events.get(index) else {
            break;
        };

        let is_dropped_tag = match event {
            MarkdownEvent::SubstitutedText(substituted) => substituted.is_empty(),
            MarkdownEvent::InlineHtml => true,
            MarkdownEvent::Start(tag) => {
                depth += 1;
                if matches!(tag, MarkdownTag::Link { .. }) {
                    link_depth += 1;
                }
                false
            }
            MarkdownEvent::End(tag) => {
                depth = depth.saturating_sub(1);
                if matches!(tag, MarkdownTagEnd::Link) {
                    link_depth = link_depth.saturating_sub(1);
                }
                open.retain(|open| open.depth <= depth);
                false
            }
            _ => false,
        };

        if !is_dropped_tag {
            continue;
        }

        let Some((name, destination, closing)) = text
            .get(range.clone())
            .and_then(|html| inline_html_style_tag(html))
        else {
            continue;
        };

        // A link inside a link has no rendering. Inside a Markdown link both
        // ends of an `<a>` keep being dropped; inside another `<a>` only the
        // inner opening tag is, so the first `</a>` still closes the outer one.
        if name == "a" && (link_depth > 0 || (!closing && open.iter().any(|open| open.name == "a")))
        {
            continue;
        }

        if closing {
            if open
                .last()
                .is_some_and(|open| open.name == name && open.depth == depth)
                && let Some(open) = open.pop()
            {
                pairs.push((open, index));
            }
        } else {
            open.push(OpenInlineTag {
                name,
                destination,
                index,
                depth,
            });
        }
    }

    for (open, close_index) in pairs {
        let open_index = open.index;
        let Some(open_range) = events.get(open_index).map(|(range, _)| range.clone()) else {
            continue;
        };
        let Some(close_range) = events.get(close_index).map(|(range, _)| range.clone()) else {
            continue;
        };
        // Both ends carry the whole span the way pulldown's own emphasis events
        // do, so copying part of it still re-adds the tags around the selection.
        let span = open_range.start..close_range.end;

        let pair_events = match open.name {
            // A Markdown link between the two tags is only reached now: it
            // opens and closes while the `<a>` is on the stack, so `link_depth`
            // was zero at both ends. Wrapping it would put its text inside two
            // links, and `link_for_source_index` answers with the first one it
            // finds — the `<a>`, whose destination is not the one the Markdown
            // link is written to open.
            "a" if contains_link(events, open_index, close_index) => None,
            "a" => open.destination.map(|destination| {
                (
                    MarkdownEvent::Start(MarkdownTag::Link {
                        link_type: LinkType::Inline,
                        dest_url: SharedString::from(destination),
                        title: SharedString::default(),
                        id: SharedString::default(),
                    }),
                    MarkdownEvent::End(MarkdownTagEnd::Link),
                )
            }),
            name => inline_html_style_events(name),
        };

        if let Some((start_event, end_event)) = pair_events {
            if let Some(event) = events.get_mut(open_index) {
                *event = (span.clone(), start_event);
            }
            if let Some(event) = events.get_mut(close_index) {
                *event = (span, end_event);
            }
        } else {
            substitute_unicode_script(events, open.name, open_index, close_index, text);
        }
    }
}

/// Whether a link starts strictly between two events.
fn contains_link(
    events: &[(Range<usize>, MarkdownEvent)],
    open_index: usize,
    close_index: usize,
) -> bool {
    events
        .get(open_index + 1..close_index)
        .into_iter()
        .flatten()
        .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Link { .. })))
}

/// Rewrites `<sub>`/`<sup>` contents to the matching Unicode characters, the
/// only way the preview can show them smaller: a line is laid out at a single
/// font size, so a run cannot shrink or leave the baseline on its own.
fn substitute_unicode_script(
    events: &mut [(Range<usize>, MarkdownEvent)],
    name: &str,
    open_index: usize,
    close_index: usize,
    text: &str,
) {
    // Every event keeps the characters that came from its own source range, and
    // the mapping is one character for one character, so each is rewritten on
    // its own. Anything that is not plain text, or that Unicode has no
    // character for, leaves the whole span alone.
    let mut substitutions = Vec::new();
    for index in open_index + 1..close_index {
        let Some((range, event)) = events.get(index) else {
            return;
        };
        let contents = match event {
            MarkdownEvent::Text => text.get(range.clone()).unwrap_or_default(),
            MarkdownEvent::SubstitutedText(substituted) => substituted.as_str(),
            _ => return,
        };
        let Some(substituted) = html::html_parser::unicode_script_for_tag(contents, name) else {
            return;
        };
        substitutions.push((index, substituted));
    }

    for (index, substituted) in substitutions {
        if let Some((_, event)) = events.get_mut(index) {
            *event = MarkdownEvent::SubstitutedText(substituted);
        }
    }
}

fn build_footnote_definitions(
    events: &[(Range<usize>, MarkdownEvent)],
) -> HashMap<SharedString, usize> {
    let mut definitions = HashMap::default();
    let mut current_label: Option<SharedString> = None;

    for (range, event) in events {
        match event {
            MarkdownEvent::Start(MarkdownTag::FootnoteDefinition(label)) => {
                current_label = Some(label.clone());
            }
            MarkdownEvent::End(MarkdownTagEnd::FootnoteDefinition) => {
                current_label = None;
            }
            MarkdownEvent::Text if current_label.is_some() => {
                if let Some(label) = current_label.take() {
                    definitions.entry(label).or_insert(range.start);
                }
            }
            _ => {}
        }
    }

    definitions
}

pub fn parse_links_only(text: &str) -> Vec<(Range<usize>, MarkdownEvent)> {
    let mut events = Vec::new();
    let mut finder = LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    let mut text_range = Range {
        start: 0,
        end: text.len(),
    };
    for link in finder.links(text) {
        let link_range = link.start()..link.end();

        if link_range.start > text_range.start {
            events.push((text_range.start..link_range.start, MarkdownEvent::Text));
        }

        events.push((
            link_range.clone(),
            MarkdownEvent::Start(MarkdownTag::Link {
                link_type: LinkType::Autolink,
                dest_url: SharedString::from(link.as_str().to_string()),
                title: SharedString::default(),
                id: SharedString::default(),
            }),
        ));
        events.push((link_range.clone(), MarkdownEvent::Text));
        events.push((link_range.clone(), MarkdownEvent::End(MarkdownTagEnd::Link)));

        text_range.start = link_range.end;
    }

    if text_range.end > text_range.start {
        events.push((text_range, MarkdownEvent::Text));
    }

    events
}

/// A static-lifetime equivalent of pulldown_cmark::Event so we can cache the
/// parse result for rendering without resorting to unsafe lifetime coercion.
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownEvent {
    /// Start of a tagged element. Events that are yielded after this event
    /// and before its corresponding `End` event are inside this element.
    /// Start and end events are guaranteed to be balanced.
    Start(MarkdownTag),
    /// End of a tagged element.
    End(MarkdownTagEnd),
    /// Text that uses the associated range from the markdown source.
    Text,
    /// Text that differs from the markdown source - typically due to substitution of HTML entities
    /// and smart punctuation.
    SubstitutedText(String),
    /// An inline code node.
    Code,
    /// An inline code node that differs from the markdown source due to escape decoding.
    SubstitutedCode(String),
    /// An HTML node.
    Html,
    /// An inline HTML node.
    InlineHtml,
    /// Start of an inline HTML formatting span that markdown has no tag of its
    /// own for, such as `<u>` or `<kbd>`. Both ends carry the whole span's range.
    InlineHtmlStyleStart(InlineHtmlStyle),
    /// End of an [`MarkdownEvent::InlineHtmlStyleStart`] span.
    InlineHtmlStyleEnd(InlineHtmlStyle),
    /// A reference to a footnote with given label, which may or may not be defined
    /// by an event with a `Tag::FootnoteDefinition` tag. Definitions and references to them may
    /// occur in any order.
    FootnoteReference(SharedString),
    /// A soft line break.
    SoftBreak,
    /// A hard line break.
    HardBreak,
    /// A horizontal ruler.
    Rule,
    /// A task list marker, rendered as a checkbox in HTML. Contains a true when it is checked.
    TaskListMarker(bool),
    /// Start of a root-level block (a top-level structural element like a paragraph, heading, list, etc.).
    RootStart,
    /// End of a root-level block. Contains the root block index.
    RootEnd(usize),
}

/// Text styles an inline HTML tag can ask for that markdown itself cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineHtmlStyle {
    /// `<u>` and `<ins>`.
    Underline,
    /// `<code>` and the other monospace tags.
    Code,
}

/// Tags for elements that can contain other elements.
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownTag {
    /// A paragraph of text and other inline elements.
    Paragraph,

    /// A heading, with optional identifier, classes and custom attributes.
    /// The identifier is prefixed with `#` and the last one in the attributes
    /// list is chosen, classes are prefixed with `.` and custom attributes
    /// have no prefix and can optionally have a value (`myattr` o `myattr=myvalue`).
    Heading {
        level: HeadingLevel,
        id: Option<SharedString>,
        classes: Vec<SharedString>,
        /// The first item of the tuple is the attr and second one the value.
        attrs: Vec<(SharedString, Option<SharedString>)>,
    },

    BlockQuote(Option<pulldown_cmark::BlockQuoteKind>),

    /// A code block.
    CodeBlock {
        kind: CodeBlockKind,
        metadata: CodeBlockMetadata,
    },

    /// A HTML block.
    HtmlBlock,

    /// A list. If the list is ordered the field indicates the number of the first item.
    /// Contains only list items.
    List(Option<u64>), // TODO: add delim and tight for ast (not needed for html)

    /// A list item.
    Item,

    /// A footnote definition. The value contained is the footnote's label by which it can
    /// be referred to.
    FootnoteDefinition(SharedString),

    /// A table. Contains a vector describing the text-alignment for each of its columns.
    Table(Vec<Alignment>),

    /// A table header. Contains only `TableCell`s. Note that the table body starts immediately
    /// after the closure of the `TableHead` tag. There is no `TableBody` tag.
    TableHead,

    /// A table row. Is used both for header rows as body rows. Contains only `TableCell`s.
    TableRow,
    TableCell,

    // span-level tags
    Emphasis,
    Strong,
    Strikethrough,
    Superscript,
    Subscript,

    /// A link.
    Link {
        link_type: LinkType,
        dest_url: SharedString,
        title: SharedString,
        /// Identifier of reference links, e.g. `world` in the link `[hello][world]`.
        id: SharedString,
    },

    /// An image. The first field is the link type, the second the destination URL and the third is a title,
    /// the fourth is the link identifier.
    Image {
        link_type: LinkType,
        dest_url: SharedString,
        title: SharedString,
        /// Identifier of reference links, e.g. `world` in the link `[hello][world]`.
        id: SharedString,
    },

    /// A metadata block.
    MetadataBlock(MetadataBlockKind),

    DefinitionList,
    DefinitionListTitle,
    DefinitionListDefinition,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CodeBlockKind {
    Indented,
    /// "Fenced" means "surrounded by triple backticks."
    /// There can optionally be either a language after the backticks (like in traditional Markdown)
    /// or, if an agent is specifying a path for a source location in the project, it can be a PathRange,
    /// e.g. ```path/to/foo.rs#L123-456 instead of ```rust
    Fenced,
    FencedLang(SharedString),
    FencedSrc(PathWithRange),
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct CodeBlockMetadata {
    pub content_range: Range<usize>,
    pub line_count: usize,
    pub is_fenced_closed: bool,
}

/// Range of the content of an inline code span within its source text, i.e. the
/// span without its surrounding backtick delimiters.
pub fn extract_code_content_range(text: &str) -> Range<usize> {
    let text_len = text.len();
    if text_len == 0 {
        return 0..0;
    }

    let start_ticks = text.chars().take_while(|&c| c == '`').count();

    if start_ticks == 0 || start_ticks > text_len {
        return 0..text_len;
    }

    let end_ticks = text.chars().rev().take_while(|&c| c == '`').count();

    if end_ticks != start_ticks || text_len < start_ticks + end_ticks {
        return 0..text_len;
    }

    start_ticks..text_len - end_ticks
}

pub(crate) fn extract_code_block_content_range(text: &str) -> Range<usize> {
    let mut range = 0..text.len();
    let Some(fence_character) = text
        .as_bytes()
        .first()
        .copied()
        .filter(|character| matches!(character, b'`' | b'~'))
    else {
        return range;
    };
    let opening_fence_len = text
        .bytes()
        .take_while(|character| *character == fence_character)
        .count();
    if opening_fence_len < 3 {
        return range;
    }

    range.start += opening_fence_len;
    if let Some(newline_ix) = text[range.clone()].find('\n') {
        range.start += newline_ix + 1;
    }

    let text_without_line_ending =
        text.trim_end_matches(|character| matches!(character, '\r' | '\n'));
    let closing_line_start = text_without_line_ending
        .rfind('\n')
        .map_or(0, |newline_ix| newline_ix + 1);
    if closing_line_start >= range.start {
        let closing_line = &text_without_line_ending[closing_line_start..];
        let closing_fence = closing_line.trim_start_matches(' ');
        let indentation_len = closing_line.len() - closing_fence.len();
        let closing_fence_len = closing_fence
            .bytes()
            .take_while(|character| *character == fence_character)
            .count();
        let trailing_characters = &closing_fence[closing_fence_len..];
        if indentation_len <= 3
            && closing_fence_len >= opening_fence_len
            && trailing_characters
                .bytes()
                .all(|character| matches!(character, b' ' | b'\t'))
        {
            range.end = closing_line_start;
        }
    }

    if range.start > range.end {
        range.end = range.start;
    }
    range
}

#[cfg(test)]
mod tests {
    use super::MarkdownEvent::*;
    use super::MarkdownTag::*;
    use super::*;

    const CONDITIONAL_OPTIONS: Options = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    const UNWANTED_OPTIONS: Options = Options::ENABLE_MATH
        .union(Options::ENABLE_DEFINITION_LIST)
        .union(Options::ENABLE_WIKILINKS);

    #[test]
    fn all_options_considered() {
        // The purpose of this is to fail when new options are added to pulldown_cmark, so that they
        // can be evaluated for inclusion.
        assert_eq!(
            PARSE_OPTIONS
                .union(CONDITIONAL_OPTIONS)
                .union(UNWANTED_OPTIONS),
            Options::all()
        );
    }

    #[test]
    fn wanted_and_unwanted_options_disjoint() {
        assert_eq!(
            PARSE_OPTIONS
                .union(CONDITIONAL_OPTIONS)
                .intersection(UNWANTED_OPTIONS),
            Options::empty()
        );
    }

    #[test]
    fn test_yaml_style_metadata_block() {
        assert_eq!(
            parse_markdown_with_options("---\ntitle: Post\n---\n# Heading", false, false, true),
            ParsedMarkdownData {
                events: vec![
                    (0..19, RootStart),
                    (0..19, Start(MetadataBlock(MetadataBlockKind::YamlStyle))),
                    (4..16, Text),
                    (
                        0..19,
                        End(MarkdownTagEnd::MetadataBlock(MetadataBlockKind::YamlStyle))
                    ),
                    (0..19, RootEnd(0)),
                    (20..29, RootStart),
                    (
                        20..29,
                        Start(Heading {
                            level: HeadingLevel::H1,
                            id: None,
                            classes: Vec::new(),
                            attrs: Vec::new(),
                        })
                    ),
                    (22..29, Text),
                    (20..29, End(MarkdownTagEnd::Heading(HeadingLevel::H1))),
                    (20..29, RootEnd(1)),
                ],
                root_block_starts: vec![0, 20],
                metadata_blocks: BTreeMap::from_iter([(
                    0,
                    ParsedMetadataBlock {
                        content_range: 4..16,
                        rows: Some(vec![MetadataRow {
                            key: 4..9,
                            value: 11..15,
                        }]),
                    },
                )]),
                ..Default::default()
            }
        )
    }

    #[test]
    fn test_metadata_block_text_is_verbatim() {
        let parsed =
            parse_markdown_with_options("---\nurl: https://zed.dev\n---\nBody", false, false, true);
        assert!(
            parsed
                .events
                .iter()
                .all(|(_, event)| !matches!(event, Start(Link { .. })))
        );
    }

    #[test]
    fn test_metadata_blocks_store_table_rows() {
        let parsed = parse_markdown_with_options(
            "---\ntitle: Post\nauthor: Zed\n---\nBody",
            false,
            false,
            true,
        );

        assert_eq!(
            parsed.metadata_blocks,
            BTreeMap::from_iter([(
                0,
                ParsedMetadataBlock {
                    content_range: 4..28,
                    rows: Some(vec![
                        MetadataRow {
                            key: 4..9,
                            value: 11..15,
                        },
                        MetadataRow {
                            key: 16..22,
                            value: 24..27,
                        },
                    ]),
                },
            )])
        );
    }

    #[test]
    fn test_metadata_blocks_store_fallback_for_nested_yaml() {
        let parsed =
            parse_markdown_with_options("---\ntags:\n  - zed\n---\nBody", false, false, true);

        assert_eq!(
            parsed.metadata_blocks,
            BTreeMap::from_iter([(
                0,
                ParsedMetadataBlock {
                    content_range: 4..18,
                    rows: None,
                },
            )])
        );
    }

    #[test]
    fn test_metadata_table_rows_parse_simple_colon_pairs() {
        let source = "title: Post\nauthor: Zed\n";
        let Some(rows) = parse_metadata_table_rows(source, 0..source.len()) else {
            panic!("expected metadata rows");
        };
        let pairs = rows
            .into_iter()
            .map(|row| (&source[row.key], &source[row.value]))
            .collect::<Vec<_>>();

        assert_eq!(pairs, vec![("title", "Post"), ("author", "Zed")]);
    }

    #[test]
    fn test_metadata_table_rows_reject_non_simple_colon_pairs() {
        for source in [
            "tags:\n  - zed\n",
            "title = Post\n",
            "title:\n",
            "title:   \n",
            ": Post\n",
            " title: Post\n",
            "\n",
        ] {
            assert!(parse_metadata_table_rows(source, 0..source.len()).is_none());
        }
    }

    #[test]
    fn test_trim_metadata_range_returns_valid_empty_range() {
        let source = "key:   \n";
        let trimmed = trim_metadata_range(source, 4..7);

        assert_eq!(trimmed, 7..7);
        assert!(source[trimmed].is_empty());
    }

    #[test]
    fn test_html_comments() {
        assert_eq!(
            parse_markdown_with_options(
                "  <!--\nrdoc-file=string.c\n-->\nReturns",
                false,
                false,
                false
            ),
            ParsedMarkdownData {
                events: vec![
                    (2..30, RootStart),
                    (2..30, Start(HtmlBlock)),
                    (2..2, SubstitutedText("  ".into())),
                    (2..7, Html),
                    (7..26, Html),
                    (26..30, Html),
                    (2..30, End(MarkdownTagEnd::HtmlBlock)),
                    (2..30, RootEnd(0)),
                    (30..37, RootStart),
                    (30..37, Start(Paragraph)),
                    (30..37, Text),
                    (30..37, End(MarkdownTagEnd::Paragraph)),
                    (30..37, RootEnd(1)),
                ],
                root_block_starts: vec![2, 30],
                ..Default::default()
            }
        )
    }

    #[test]
    fn test_plain_urls_and_escaped_text() {
        assert_eq!(
            parse_markdown_with_options(
                "&nbsp;&nbsp; https://some.url some \\`&#9658;\\` text",
                false,
                false,
                false,
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..51, RootStart),
                    (0..51, Start(Paragraph)),
                    (0..6, SubstitutedText("\u{a0}".into())),
                    (6..12, SubstitutedText("\u{a0}".into())),
                    (12..13, Text),
                    (
                        13..29,
                        Start(Link {
                            link_type: LinkType::Autolink,
                            dest_url: "https://some.url".into(),
                            title: "".into(),
                            id: "".into(),
                        })
                    ),
                    (13..29, Text),
                    (13..29, End(MarkdownTagEnd::Link)),
                    (29..35, Text),
                    (36..37, Text), // Escaped backtick
                    (37..44, SubstitutedText("►".into())),
                    (45..46, Text), // Escaped backtick
                    (46..51, Text),
                    (0..51, End(MarkdownTagEnd::Paragraph)),
                    (0..51, RootEnd(0)),
                ],
                root_block_starts: vec![0],
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_incomplete_link() {
        assert_eq!(
            parse_markdown_with_options(
                "You can use the [GitHub Search API](https://docs.github.com/en",
                false,
                false,
                false,
            )
            .events,
            vec![
                (0..62, RootStart),
                (0..62, Start(Paragraph)),
                (0..16, Text),
                (16..17, Text),
                (17..34, Text),
                (34..35, Text),
                (35..36, Text),
                (
                    36..62,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://docs.github.com/en".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (36..62, Text),
                (36..62, End(MarkdownTagEnd::Link)),
                (0..62, End(MarkdownTagEnd::Paragraph)),
                (0..62, RootEnd(0)),
            ],
        );
    }

    #[test]
    fn test_smart_punctuation() {
        assert_eq!(
            parse_markdown_with_options(
                "-- --- ... \"double quoted\" 'single quoted' ----------",
                false,
                false,
                false,
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..53, RootStart),
                    (0..53, Start(Paragraph)),
                    (0..2, SubstitutedText("–".into())),
                    (2..3, Text),
                    (3..6, SubstitutedText("—".into())),
                    (6..7, Text),
                    (7..10, SubstitutedText("…".into())),
                    (10..11, Text),
                    (11..12, SubstitutedText("\u{201c}".into())),
                    (12..25, Text),
                    (25..26, SubstitutedText("\u{201d}".into())),
                    (26..27, Text),
                    (27..28, SubstitutedText("\u{2018}".into())),
                    (28..41, Text),
                    (41..42, SubstitutedText("\u{2019}".into())),
                    (42..43, Text),
                    (43..53, SubstitutedText("–––––".into())),
                    (0..53, End(MarkdownTagEnd::Paragraph)),
                    (0..53, RootEnd(0)),
                ],
                root_block_starts: vec![0],
                ..Default::default()
            }
        )
    }

    #[test]
    fn test_code_block_metadata() {
        assert_eq!(
            parse_markdown_with_options(
                "```rust\nfn main() {\n let a = 1;\n}\n```",
                false,
                false,
                false
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..37, RootStart),
                    (
                        0..37,
                        Start(CodeBlock {
                            kind: CodeBlockKind::FencedLang("rust".into()),
                            metadata: CodeBlockMetadata {
                                content_range: 8..34,
                                line_count: 3,
                                is_fenced_closed: true,
                            }
                        })
                    ),
                    (8..34, Text),
                    (0..37, End(MarkdownTagEnd::CodeBlock)),
                    (0..37, RootEnd(0)),
                ],
                language_names: {
                    let mut h = HashSet::default();
                    h.insert("rust".into());
                    h
                },
                root_block_starts: vec![0],
                ..Default::default()
            }
        );
        assert_eq!(
            parse_markdown_with_options("    fn main() {}", false, false, false),
            ParsedMarkdownData {
                events: vec![
                    (4..16, RootStart),
                    (
                        4..16,
                        Start(CodeBlock {
                            kind: CodeBlockKind::Indented,
                            metadata: CodeBlockMetadata {
                                content_range: 4..16,
                                line_count: 1,
                                is_fenced_closed: false,
                            }
                        })
                    ),
                    (4..16, Text),
                    (4..16, End(MarkdownTagEnd::CodeBlock)),
                    (4..16, RootEnd(0)),
                ],
                root_block_starts: vec![4],
                ..Default::default()
            }
        );

        for markdown in ["```mermaid\ngraph TD;\n~~~", "~~~~mermaid\ngraph TD;\n~~~"] {
            let parsed = parse_markdown_with_options(markdown, false, false, false);
            let metadata = parsed.events.iter().find_map(|(_, event)| match event {
                Start(CodeBlock { metadata, .. }) => Some(metadata),
                _ => None,
            });
            assert_eq!(
                metadata.map(|metadata| metadata.is_fenced_closed),
                Some(false)
            );
        }
    }

    fn assert_code_block_does_not_emit_links(markdown: &str) {
        let parsed = parse_markdown_with_options(markdown, false, false, false);
        let mut code_block_depth = 0;
        let mut code_block_count = 0;
        let mut saw_text_inside_code_block = false;

        for (_, event) in &parsed.events {
            match event {
                Start(CodeBlock { .. }) => {
                    code_block_depth += 1;
                    code_block_count += 1;
                }
                End(MarkdownTagEnd::CodeBlock) => {
                    assert!(
                        code_block_depth > 0,
                        "encountered a code block end without a matching start"
                    );
                    code_block_depth -= 1;
                }
                Start(Link { .. }) | End(MarkdownTagEnd::Link) => {
                    assert_eq!(
                        code_block_depth, 0,
                        "code blocks should not emit link events"
                    );
                }
                Text | SubstitutedText(_) if code_block_depth > 0 => {
                    saw_text_inside_code_block = true;
                }
                _ => {}
            }
        }

        assert_eq!(code_block_count, 1, "expected exactly one code block");
        assert_eq!(code_block_depth, 0, "unterminated code block");
        assert!(
            saw_text_inside_code_block,
            "expected text inside the code block"
        );
    }

    #[test]
    fn test_code_blocks_do_not_autolink_urls() {
        assert_code_block_does_not_emit_links("```txt\nhttps://example.com\n```");
        assert_code_block_does_not_emit_links("    https://example.com");
        assert_code_block_does_not_emit_links(
            "```txt\r\nhttps:/\\/example.com\r\nhttps://example&#46;com\r\n```",
        );
        assert_code_block_does_not_emit_links(
            "    https:/\\/example.com\r\n    https://example&#46;com",
        );
    }

    #[test]
    fn test_metadata_blocks_are_root_blocks() {
        assert_eq!(
            parse_markdown_with_options(
                "+++\ntitle = \"Example\"\n+++\n\nParagraph",
                false,
                false,
                true
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..25, RootStart),
                    (0..25, Start(MetadataBlock(MetadataBlockKind::PlusesStyle))),
                    (4..22, Text),
                    (
                        0..25,
                        End(MarkdownTagEnd::MetadataBlock(
                            MetadataBlockKind::PlusesStyle
                        ))
                    ),
                    (0..25, RootEnd(0)),
                    (27..36, RootStart),
                    (27..36, Start(Paragraph)),
                    (27..36, Text),
                    (27..36, End(MarkdownTagEnd::Paragraph)),
                    (27..36, RootEnd(1)),
                ],
                root_block_starts: vec![0, 27],
                metadata_blocks: BTreeMap::from_iter([(
                    0,
                    ParsedMetadataBlock {
                        content_range: 4..22,
                        rows: None,
                    },
                )]),
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_metadata_blocks_are_omitted_by_default() {
        assert_eq!(
            parse_markdown_with_options(
                "+++\ntitle = \"Example\"\n+++\n\nParagraph",
                false,
                false,
                false
            ),
            ParsedMarkdownData {
                events: vec![
                    (27..36, RootStart),
                    (27..36, Start(Paragraph)),
                    (27..36, Text),
                    (27..36, End(MarkdownTagEnd::Paragraph)),
                    (27..36, RootEnd(0)),
                ],
                root_block_starts: vec![27],
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_table_checkboxes_remain_text_in_cells() {
        let markdown = "\
| Done | Task    |
|------|---------|
| [x]  | Fix bug |
| [ ]  | Add feature |";
        let parsed = parse_markdown_with_options(markdown, false, false, false);

        let mut in_table = false;
        let mut saw_task_list_marker = false;
        let mut cell_texts = Vec::new();
        let mut current_cell = String::new();

        for (range, event) in &parsed.events {
            match event {
                Start(Table(_)) => in_table = true,
                End(MarkdownTagEnd::Table) => in_table = false,
                Start(TableCell) => current_cell.clear(),
                End(MarkdownTagEnd::TableCell) => {
                    if in_table {
                        cell_texts.push(current_cell.clone());
                    }
                }
                Text if in_table => current_cell.push_str(&markdown[range.clone()]),
                TaskListMarker(_) if in_table => saw_task_list_marker = true,
                _ => {}
            }
        }

        let checkbox_cells: Vec<&str> = cell_texts
            .iter()
            .map(|cell| cell.trim())
            .filter(|cell| *cell == "[x]" || *cell == "[X]" || *cell == "[ ]")
            .collect();

        assert!(
            !saw_task_list_marker,
            "Table checkboxes should remain text, not task-list markers"
        );
        assert_eq!(checkbox_cells, vec!["[x]", "[ ]"]);
    }

    #[test]
    fn test_extract_code_content_range() {
        let input = "```let x = 5;```";
        assert_eq!(extract_code_content_range(input), 3..13);

        let input = "``let x = 5;``";
        assert_eq!(extract_code_content_range(input), 2..12);

        let input = "`let x = 5;`";
        assert_eq!(extract_code_content_range(input), 1..11);

        let input = "plain text";
        assert_eq!(extract_code_content_range(input), 0..10);

        let input = "``let x = 5;`";
        assert_eq!(extract_code_content_range(input), 0..13);
    }

    #[test]
    fn test_inline_code_substitutes_escaped_pipes() {
        let markdown = r"| Pattern |
| --- |
| `a\|b` |";
        let parsed = parse_markdown_with_options(markdown, false, false, false);
        let code_range = {
            let start = markdown.find(r"a\|b").expect("inline code source");
            start..start + r"a\|b".len()
        };

        assert!(
            parsed
                .events
                .iter()
                .any(|(range, event)| range == &code_range
                    && event == &SubstitutedCode("a|b".into())),
            "expected escaped pipe in table inline code to render as decoded inline code: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_inline_code_keeps_escaped_pipes_outside_tables() {
        let markdown = r"`a\|b`";
        let parsed = parse_markdown_with_options(markdown, false, false, false);

        assert!(
            parsed
                .events
                .iter()
                .any(|(range, event)| range == &(1..5) && event == &Code),
            "expected escaped pipe outside a table to remain normal inline code: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_extract_code_block_content_range() {
        let input = "```rust\nlet x = 5;\n```";
        assert_eq!(extract_code_block_content_range(input), 8..19);

        let input = "plain text";
        assert_eq!(extract_code_block_content_range(input), 0..10);

        let input = "```python\nprint('hello')\nprint('world')\n```";
        assert_eq!(extract_code_block_content_range(input), 10..40);

        let input = "~~~~mermaid\ngraph TD;\n~~~~";
        let content_range = extract_code_block_content_range(input);
        assert_eq!(&input[content_range], "graph TD;\n");

        let input = "~~~mermaid\ngraph TD;\n    ~~~~";
        let content_range = extract_code_block_content_range(input);
        assert_eq!(&input[content_range], "graph TD;\n    ~~~~");

        let input = "~~~mermaid\ngraph TD;\n   ~~~~ \t";
        let content_range = extract_code_block_content_range(input);
        assert_eq!(&input[content_range], "graph TD;\n");

        // Malformed input
        let input = "`````";
        assert_eq!(extract_code_block_content_range(input), 5..5);
    }

    #[test]
    fn test_footnotes() {
        let parsed = parse_markdown_with_options(
            "Text with a footnote[^1] and some more text.\n\n[^1]: This is the footnote content.",
            false,
            false,
            false,
        );
        assert_eq!(
            parsed.events,
            vec![
                (0..45, RootStart),
                (0..45, Start(Paragraph)),
                (0..20, Text),
                (20..24, FootnoteReference("1".into())),
                (24..44, Text),
                (0..45, End(MarkdownTagEnd::Paragraph)),
                (0..45, RootEnd(0)),
                (46..81, RootStart),
                (46..81, Start(FootnoteDefinition("1".into()))),
                (52..81, Start(Paragraph)),
                (52..81, Text),
                (52..81, End(MarkdownTagEnd::Paragraph)),
                (46..81, End(MarkdownTagEnd::FootnoteDefinition)),
                (46..81, RootEnd(1)),
            ]
        );
        assert_eq!(parsed.footnote_definitions.len(), 1);
        assert_eq!(parsed.footnote_definitions.get("1").copied(), Some(52));
    }

    #[test]
    fn test_footnote_definitions_multiple() {
        let parsed = parse_markdown_with_options(
            "Text[^a] and[^b].\n\n[^a]: First.\n\n[^b]: Second.",
            false,
            false,
            false,
        );
        assert_eq!(parsed.footnote_definitions.len(), 2);
        assert!(parsed.footnote_definitions.contains_key("a"));
        assert!(parsed.footnote_definitions.contains_key("b"));
    }

    #[test]
    fn test_links_split_across_fragments() {
        // This test verifies that links split across multiple text fragments due to escaping or other issues
        // are correctly detected and processed
        // Note: In real usage, pulldown_cmark creates separate text events for the escaped character
        // We're verifying our parser can handle this correctly
        assert_eq!(
            parse_markdown_with_options(
                "https:/\\/example.com is equivalent to https://example&#46;com!",
                false,
                false,
                false,
            )
            .events,
            vec![
                (0..62, RootStart),
                (0..62, Start(Paragraph)),
                (
                    0..20,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://example.com".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (0..7, Text),
                (8..20, Text),
                (0..20, End(MarkdownTagEnd::Link)),
                (20..38, Text),
                (
                    38..61,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://example.com".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (38..53, Text),
                (53..58, SubstitutedText(".".into())),
                (58..61, Text),
                (38..61, End(MarkdownTagEnd::Link)),
                (61..62, Text),
                (0..62, End(MarkdownTagEnd::Paragraph)),
                (0..62, RootEnd(0)),
            ],
        );

        assert_eq!(
            parse_markdown_with_options(
                "Visit https://example.com/cat\\/é&#8205;☕ for coffee!",
                false,
                false,
                false,
            )
            .events,
            [
                (0..55, RootStart),
                (0..55, Start(Paragraph)),
                (0..6, Text),
                (
                    6..43,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://example.com/cat/é\u{200d}☕".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (6..29, Text),
                (30..33, Text),
                (33..40, SubstitutedText("\u{200d}".into())),
                (40..43, Text),
                (6..43, End(MarkdownTagEnd::Link)),
                (43..55, Text),
                (0..55, End(MarkdownTagEnd::Paragraph)),
                (0..55, RootEnd(0)),
            ]
        );
    }

    #[test]
    fn test_heading_slugs() {
        let parsed = parse_markdown_with_options(
            "# Hello World\n\n## Code `block`\n\n### Third Level\n\n#### Fourth Level\n\n## Hello World",
            false,
            true,
            false,
        );
        assert_eq!(parsed.heading_slugs.len(), 5);
        assert!(parsed.heading_slugs.contains_key("hello-world"));
        assert!(parsed.heading_slugs.contains_key("code-block"));
        assert!(parsed.heading_slugs.contains_key("third-level"));
        assert!(parsed.heading_slugs.contains_key("fourth-level"));
        assert!(parsed.heading_slugs.contains_key("hello-world-1"));
    }

    #[test]
    fn test_heading_source_index_for_slug() {
        let parsed = parse_markdown_with_options(
            "# Duplicate\n\nText\n\n## Duplicate\n\nMore text",
            false,
            true,
            false,
        );
        let first = parsed.heading_slugs.get("duplicate").copied();
        let second = parsed.heading_slugs.get("duplicate-1").copied();
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(first.expect("first slug missing") < second.expect("second slug missing"));
    }

    #[test]
    fn test_heading_slug_collision_with_dedup_suffix() {
        let parsed = parse_markdown_with_options("# Foo\n\n## Foo\n\n## Foo 1", false, true, false);
        assert_eq!(parsed.heading_slugs.len(), 3);
        assert!(parsed.heading_slugs.contains_key("foo"));
        assert!(parsed.heading_slugs.contains_key("foo-1"));
        assert!(parsed.heading_slugs.contains_key("foo-1-1"));
    }

    #[test]
    fn test_gfm_alert_block_quote_kinds() {
        use pulldown_cmark::BlockQuoteKind;

        let markdown = "\n> [!NOTE]\n> A note.\n\n> [!TIP]\n> A tip.\n\n> [!IMPORTANT]\n> Important.\n\n> [!WARNING]\n> A warning.\n\n> [!CAUTION]\n> A caution.\n\n> Plain quote.\n";
        let parsed = parse_markdown_with_options(markdown, false, false, false);

        let block_quote_kinds: Vec<_> = parsed
            .events
            .iter()
            .filter_map(|(_, event)| match event {
                Start(BlockQuote(kind)) => Some(*kind),
                _ => None,
            })
            .collect();

        assert_eq!(
            block_quote_kinds,
            vec![
                Some(BlockQuoteKind::Note),
                Some(BlockQuoteKind::Tip),
                Some(BlockQuoteKind::Important),
                Some(BlockQuoteKind::Warning),
                Some(BlockQuoteKind::Caution),
                None,
            ]
        );
    }

    #[test]
    fn test_br_tag_emits_hard_break() {
        for input in [
            "hello<br>world",
            "hello<br/>world",
            "hello<br />world",
            "hello<br >world",
            "hello<BR>world",
            "hello<br class=\"x\">world",
            "hello<br class=\"x\"/>world",
        ] {
            let parsed = parse_markdown_with_options(input, true, false, false);
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            let has_empty_substituted_text = parsed.events.iter().any(|(_, event)| {
                matches!(event, MarkdownEvent::SubstitutedText(text) if text.is_empty())
            });
            assert!(has_hard_break, "<br> in \"{input}\" should emit HardBreak");
            assert!(
                !has_empty_substituted_text,
                "<br> in \"{input}\" should not produce empty SubstitutedText"
            );
        }
    }

    #[test]
    fn test_br_tag_not_a_hard_break_without_parse_html() {
        for input in ["hello<br>world", "hello<br/>world", "hello<br />world"] {
            let parsed = parse_markdown_with_options(input, false, false, false);
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            let has_inline_html = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml));
            assert!(
                !has_hard_break,
                "<br> in \"{input}\" should not emit HardBreak when parse_html is disabled"
            );
            assert!(
                has_inline_html,
                "<br> in \"{input}\" should be preserved as InlineHtml when parse_html is disabled"
            );
        }
    }

    #[test]
    fn test_br_prefixed_tag_is_not_a_hard_break() {
        for input in ["a<break>b", "a<brick>b", "a<b>bold</b>c"] {
            let parsed = parse_markdown_with_options(input, true, false, false);
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            assert!(
                !has_hard_break,
                "\"{input}\" should not be treated as a <br> hard break"
            );
        }
    }

    #[test]
    fn test_leading_formatting_tag_is_not_rendered_as_text() {
        let parsed =
            parse_markdown_with_options("<kbd>Ctrl</kbd>+<sup>1</sup>", true, false, false);

        assert!(
            !parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml)),
            "formatting tags should not reach the renderer as literal markup: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_leading_formatting_tag_is_preserved_without_parse_html() {
        let parsed = parse_markdown_with_options("<kbd>Ctrl</kbd>", false, false, false);

        assert!(
            parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml)),
            "without parse_html the tag stays inline HTML: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_inline_code_tag_still_reaches_the_renderer() {
        let parsed = parse_markdown_with_options("<code>x</code>", true, false, false);

        assert!(
            parsed.events.iter().any(|(_, event)| matches!(
                event,
                MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Code)
            )),
            "<code> is turned into a code span by the renderer: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_details_spans_are_linked_across_blocks() {
        let source = "<details>\n<summary>題</summary>\n\n本文\n\n</details>\n";
        let parsed = parse_markdown_with_options(source, true, false, false);

        let close_start = source
            .find("</details>")
            .expect("source contains the closing tag");
        let span = parsed
            .html_blocks
            .get(&0)
            .and_then(|block| block.details_span.clone())
            .expect("the opening block should be linked to the closing one");

        let hidden = parsed
            .events
            .get(span.hidden_events)
            .expect("the hidden events should be inside the event list");
        assert!(
            hidden
                .iter()
                .any(|(range, event)| matches!(event, MarkdownEvent::Text)
                    && source.get(range.clone()) == Some("本文")),
            "the body should be hidden: {hidden:?}"
        );
        assert!(
            hidden.iter().all(|(range, _)| range.start < close_start),
            "nothing from the closing block should be hidden: {hidden:?}"
        );
        assert!(
            parsed
                .html_blocks
                .get(&close_start)
                .is_some_and(|block| block.unmatched_details_closes == 1)
        );
    }

    /// The shape `markdown::InsertDetails` writes: the summary shares the
    /// opening tag's line, and `</details>` follows the closing fence with no
    /// blank line between them.
    #[test]
    fn test_details_written_by_insert_details_hides_its_code_block() {
        let source = "<details><summary>開く</summary>\n\n```\n本文\n```\n</details>\n";
        let parsed = parse_markdown_with_options(source, true, false, false);

        let close_start = source
            .find("</details>")
            .expect("source contains the closing tag");
        let span = parsed
            .html_blocks
            .get(&0)
            .and_then(|block| block.details_span.clone())
            .expect("the opening block should be linked to the closing one");

        let hidden = parsed
            .events
            .get(span.hidden_events)
            .expect("the hidden events should be inside the event list");
        assert!(
            hidden
                .iter()
                .any(|(range, event)| matches!(event, MarkdownEvent::Text)
                    && source
                        .get(range.clone())
                        .is_some_and(|text| text.trim() == "本文")),
            "the code block's body should be hidden while the disclosure is closed: {hidden:?}"
        );
        assert!(
            hidden.iter().all(|(range, _)| range.start < close_start),
            "nothing from the closing block should be hidden: {hidden:?}"
        );
        assert!(
            parsed
                .html_blocks
                .get(&close_start)
                .is_some_and(|block| block.unmatched_details_closes == 1),
            "`</details>` on the line right after the fence still closes the disclosure"
        );
    }

    #[test]
    fn test_inline_html_formatting_is_styled_in_a_plain_paragraph() {
        let source = "a <b>bold</b> <u>under</u> <kbd>K</kbd> <del>gone</del> <i>it</i>";
        let parsed = parse_markdown_with_options(source, true, false, false);
        let events = parsed
            .events
            .iter()
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();

        for expected in [
            MarkdownEvent::Start(MarkdownTag::Strong),
            MarkdownEvent::End(MarkdownTagEnd::Strong),
            MarkdownEvent::Start(MarkdownTag::Strikethrough),
            MarkdownEvent::Start(MarkdownTag::Emphasis),
            MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Underline),
            MarkdownEvent::InlineHtmlStyleEnd(InlineHtmlStyle::Underline),
            MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Code),
            MarkdownEvent::InlineHtmlStyleEnd(InlineHtmlStyle::Code),
        ] {
            assert!(
                events.contains(&expected),
                "{expected:?} is missing from {events:?}"
            );
        }

        // Both ends carry the whole span, the way pulldown spells its own
        // emphasis, so a selection that cuts through it can be rebalanced.
        let strong = parsed
            .events
            .iter()
            .find(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Strong)))
            .map(|(range, _)| range.clone())
            .expect("the bold span should be styled");
        assert_eq!(&source[strong], "<b>bold</b>");
    }

    #[test]
    fn test_unbalanced_inline_html_formatting_is_still_dropped() {
        for source in ["a <b>bold", "a </b> b", "a <b>x</i> b"] {
            let parsed = parse_markdown_with_options(source, true, false, false);
            assert!(
                !parsed
                    .events
                    .iter()
                    .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Strong))),
                "{source} has no balanced pair to style: {:?}",
                parsed.events
            );
            let starts = parsed
                .events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::Start(_)))
                .count();
            let ends = parsed
                .events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::End(_)))
                .count();
            assert_eq!(starts, ends, "{source} should keep its events balanced");
        }
    }

    #[test]
    fn test_inline_html_styling_only_looks_at_real_tags() {
        let strong_spans = |source: &str| {
            parse_markdown_with_options(source, true, false, false)
                .events
                .into_iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Strong)))
                .count()
        };

        // Attributes, casing and a `<br>` in the middle all still describe the
        // same pair of tags.
        assert_eq!(strong_spans("a <b class=\"x\">y</b>"), 1);
        assert_eq!(strong_spans("a <B>y</B>"), 1);
        assert_eq!(strong_spans("a <b>y<br>z</b>"), 1);
        assert_eq!(strong_spans("a <b><b>y</b></b>"), 2);

        // A tag that is only ever text: inside a code span, inside a fence, or
        // inside a comment. Styling any of them would eat the text around it.
        assert_eq!(strong_spans("a `<b>y</b>` z"), 0);
        assert_eq!(strong_spans("```\n<b>y</b>\n```\n"), 0);
        assert_eq!(strong_spans("a <!-- <b> --> y <!-- </b> --> z"), 0);
        assert_eq!(strong_spans("a <br> y"), 0);

        // A pair that starts inside a link's text and ends outside it would
        // cross the link's own events.
        assert_eq!(strong_spans("[a <b>x](https://example.com) y</b> z"), 0);
        assert_eq!(strong_spans("[a <b>x</b>](https://example.com)"), 1);
    }

    #[test]
    fn test_inline_html_formatting_does_not_cross_a_block_boundary() {
        // The `<b>` ends with the first paragraph, so pairing it with the
        // `</b>` of the second would leave the renderer's style stack crossed.
        let parsed = parse_markdown_with_options("a <b>x\n\ny</b> b", true, false, false);
        assert!(
            !parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Strong))),
            "{:?}",
            parsed.events
        );
    }

    /// The source text of every inline link of `source`, with the destination
    /// the renderer would open.
    fn inline_links(source: &str) -> Vec<(String, String)> {
        parse_markdown_with_options(source, true, false, false)
            .events
            .into_iter()
            .filter_map(|(range, event)| match event {
                MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. }) => {
                    Some((dest_url.to_string(), source.get(range)?.to_string()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_an_inline_html_anchor_in_a_plain_paragraph_becomes_a_link() {
        let source = "素の <a href=\"https://example.com/x\">リンク</a> です";

        assert_eq!(
            inline_links(source),
            vec![(
                "https://example.com/x".to_string(),
                "<a href=\"https://example.com/x\">リンク</a>".to_string()
            )],
            "the whole span is carried so a selection can be rebalanced"
        );

        let events = parse_markdown_with_options(source, true, false, false).events;
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::End(MarkdownTagEnd::Link)))
                .count(),
            1,
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml)),
            "the tags themselves stop being rendered as text: {events:?}"
        );
    }

    #[test]
    fn test_an_inline_html_anchor_reads_its_href_however_it_is_written() {
        for (source, expected) in [
            ("<a href=\"x\">t</a>", "x"),
            ("<a href='x'>t</a>", "x"),
            ("<a href=x>t</a>", "x"),
            ("<a HREF=\"x\">t</a>", "x"),
            ("<a title=\"t\" href=\"x\" download>t</a>", "x"),
            ("<a class=\"a b\" href=\"x\">t</a>", "x"),
            (
                "<a href=\"https://example.com/?a=1&amp;b=2\">t</a>",
                "https://example.com/?a=1&b=2",
            ),
            ("<a href=\"./notes:1.md\">t</a>", "./notes:1.md"),
            ("<a href=\"#fn-1\">t</a>", "#fn-1"),
        ] {
            let links = inline_links(source);
            assert_eq!(
                links.first().map(|(destination, _)| destination.as_str()),
                Some(expected),
                "{source}"
            );
        }
    }

    #[test]
    fn test_an_inline_html_anchor_that_cannot_be_opened_is_still_dropped() {
        for source in [
            "<a href=\"javascript:alert(1)\">t</a>",
            "<a href=\"JavaScript:alert(1)\">t</a>",
            "<a href=\"data:text/html,<b>x</b>\">t</a>",
            "<a href=\"vbscript:x\">t</a>",
            "<a href=\"\">t</a>",
            "<a href=\"   \">t</a>",
            "<a>t</a>",
            "<a name=\"x\">t</a>",
            "<a href>t</a>",
        ] {
            assert!(
                inline_links(source).is_empty(),
                "{source} should not become a link"
            );
        }
    }

    #[test]
    fn test_an_inline_html_anchor_never_nests_inside_another_link() {
        // The Markdown link already owns the text, so its `<a>` stays dropped
        // and the only link is the Markdown one.
        assert_eq!(
            inline_links("[a <a href=\"inner\">b</a> c](https://example.com)"),
            vec![(
                "https://example.com".to_string(),
                "[a <a href=\"inner\">b</a> c](https://example.com)".to_string()
            )]
        );

        // Two `<a>`s in a row: the first pair is a link, the leftover `</a>` is
        // dropped, and no link ends up inside another.
        let nested = inline_links("<a href=\"outer\">a<a href=\"inner\">b</a></a>");
        assert_eq!(
            nested
                .iter()
                .map(|(destination, _)| destination.as_str())
                .collect::<Vec<_>>(),
            vec!["outer"]
        );
    }

    #[test]
    fn test_an_unpaired_inline_html_anchor_is_left_alone() {
        for source in [
            "a <a href=\"x\">b",
            "a </a> b",
            "a <a href=\"x\">b</i> c",
            "a `<a href=\"x\">b</a>` c",
            "<!-- <a href=\"x\"> --> b <!-- </a> -->",
        ] {
            let parsed = parse_markdown_with_options(source, true, false, false);
            assert!(
                inline_links(source).is_empty(),
                "{source} has no pair to rewrite"
            );
            let starts = parsed
                .events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::Start(_)))
                .count();
            let ends = parsed
                .events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::End(_)))
                .count();
            assert_eq!(starts, ends, "{source} should keep its events balanced");
        }
    }

    #[test]
    fn test_copying_inside_an_inline_html_link_keeps_the_tags() {
        let source = "素の <a href=\"https://example.com\">リンク</a> です";
        let parsed = parse_markdown_with_options(source, true, false, false);
        let start = source
            .find("リンク")
            .expect("the link text is in the source");

        assert_eq!(
            crate::selection::rebalanced_markdown_for_selection(
                source,
                &parsed.events,
                &parsed.root_block_starts,
                start..start + "リンク".len(),
            ),
            "<a href=\"https://example.com\">リンク</a>"
        );
    }

    #[test]
    fn test_an_inline_html_anchor_carries_a_multibyte_href_through() {
        for (source, expected) in [
            (
                "<a href=\"./日本語のノート.md\">t</a>",
                "./日本語のノート.md",
            ),
            ("<a href='日本語'>t</a>", "日本語"),
            ("<a href=日本語>t</a>", "日本語"),
            ("<a href=\"x?a=日&amp;b=本\">t</a>", "x?a=日&b=本"),
            ("<a href=\"&#x3042;.md\">t</a>", "あ.md"),
            // Entities this does not know stay as they were written rather
            // than being guessed at or cut in half.
            ("<a href=\"a&b\">t</a>", "a&b"),
            ("<a href=\"a&#x\">t</a>", "a&#x"),
            ("<a href=\"a&nosuchentity;b\">t</a>", "a&nosuchentity;b"),
            ("<a href=\"a&#99999999;b\">t</a>", "a&#99999999;b"),
            ("<a href=\"日&amp;本\" title=\"題\">t</a>", "日&本"),
        ] {
            assert_eq!(
                inline_links(source)
                    .first()
                    .map(|(destination, _)| destination.as_str()),
                Some(expected),
                "{source}"
            );
        }
    }

    #[test]
    fn test_an_inline_html_anchor_that_cannot_be_read_leaves_the_paragraph_alone() {
        // None of these is a tag this can make sense of, and none of them may
        // panic on the way to finding that out.
        for source in [
            "<a href=\"x>t</a>",
            "<a href='x>t</a>",
            "<a href=\"日本語>t</a>",
            "<a href=>t</a>",
            "<a href==>t</a>",
            "<a =>t</a>",
            "<a\u{3000}href=\"x\">t</a>",
        ] {
            let parsed = parse_markdown_with_options(source, true, false, false);
            let starts = parsed
                .events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::Start(_)))
                .count();
            let ends = parsed
                .events
                .iter()
                .filter(|(_, event)| matches!(event, MarkdownEvent::End(_)))
                .count();
            assert_eq!(starts, ends, "{source} should keep its events balanced");
        }
    }

    #[test]
    fn test_a_destination_that_hides_its_scheme_is_still_refused() {
        for source in [
            // Decoded before the scheme is read.
            "<a href=\"&#106;avascript:alert(1)\">t</a>",
            "<a href=\"&#x6A;avascript:alert(1)\">t</a>",
            // `open_preview_url` percent-decodes before it opens anything.
            "<a href=\"javascript%3Aalert(1)\">t</a>",
            "<a href=\"JAVASCRIPT%3aalert(1)\">t</a>",
            "<a href=\"data%3Atext/html,x\">t</a>",
            // `Url::parse` drops these before it reads the scheme.
            "<a href=\"java&#9;script:alert(1)\">t</a>",
            "<a href=\"java&#10;script:alert(1)\">t</a>",
            "<a href=\"&#13;javascript:alert(1)\">t</a>",
            "<a href=\"&#0;javascript:alert(1)\">t</a>",
            // Nothing to open.
            "<a href=\"\u{a0}\">t</a>",
        ] {
            assert!(
                inline_links(source).is_empty(),
                "{source} should not become a link"
            );
        }
    }

    #[test]
    fn test_a_destination_that_only_looks_like_a_scheme_still_links() {
        for (source, expected) in [
            ("<a href=\"./javascript:x\">t</a>", "./javascript:x"),
            ("<a href=\"?a=javascript:x\">t</a>", "?a=javascript:x"),
            ("<a href=\"#javascript:x\">t</a>", "#javascript:x"),
            ("<a href=\"notes/data:1.md\">t</a>", "notes/data:1.md"),
            (
                "<a href=\"mailto:a@example.com\">t</a>",
                "mailto:a@example.com",
            ),
            ("<a href=\"x%20y.md\">t</a>", "x%20y.md"),
        ] {
            assert_eq!(
                inline_links(source)
                    .first()
                    .map(|(destination, _)| destination.as_str()),
                Some(expected),
                "{source}"
            );
        }
    }

    #[test]
    fn test_an_inline_html_anchor_never_swallows_a_markdown_link() {
        // The `<a>` pair would otherwise span the Markdown link inside it, and
        // the renderer resolves a click to the first link whose source range
        // covers it — which would open `outer` from `[inner](inner-url)`.
        let source = "<a href=\"outer\">前 [inner](inner-url) 後</a>";
        assert_eq!(
            inline_links(source)
                .iter()
                .map(|(destination, _)| destination.as_str())
                .collect::<Vec<_>>(),
            vec!["inner-url"],
            "the Markdown link keeps its own destination"
        );
    }

    #[test]
    fn test_an_inline_html_anchor_does_not_pair_across_paragraphs() {
        let source = "<a href=\"x\">前\n\n後</a>";
        assert!(
            inline_links(source).is_empty(),
            "the tags are in different paragraphs, so there is no pair"
        );
    }

    #[test]
    fn test_an_inline_html_anchor_keeps_an_image_and_its_emphasis() {
        let source = "<a href=\"x\">**強調** と ![代替](i.png)</a>";
        let parsed = parse_markdown_with_options(source, true, false, false);

        assert_eq!(
            inline_links(source)
                .iter()
                .map(|(destination, _)| destination.as_str())
                .collect::<Vec<_>>(),
            vec!["x"]
        );
        assert!(
            parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Image { .. }))),
            "the image inside the anchor survives: {:?}",
            parsed.events
        );
        assert!(
            parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Strong))),
            "and so does the emphasis"
        );
    }

    #[test]
    fn test_sub_and_sup_in_a_paragraph_use_unicode_characters() {
        let parsed =
            parse_markdown_with_options("H<sub>2</sub>O x<sup>2</sup>", true, false, false);
        let substituted = parsed
            .events
            .iter()
            .filter_map(|(_, event)| match event {
                MarkdownEvent::SubstitutedText(text) if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            substituted,
            vec!["\u{2082}".to_string(), "\u{b2}".to_string()]
        );

        let unmapped = parse_markdown_with_options("a<sub>zz</sub>", true, false, false);
        assert!(
            !unmapped.events.iter().any(|(_, event)| matches!(
                event,
                MarkdownEvent::SubstitutedText(text) if !text.is_empty()
            )),
            "Unicode has no subscript for those letters: {:?}",
            unmapped.events
        );
    }

    #[test]
    fn test_details_inside_a_list_item_hides_only_its_own_body() {
        let source = "- item\n\n  <details>\n  <summary>\u{984c}</summary>\n\n  \u{672c}\u{6587}\n\n  </details>\n\n\u{5f8c}\u{308d}\n";
        let parsed = parse_markdown_with_options(source, true, false, false);

        let open_start = source.find("<details>").expect("the opening tag is there");
        let span = parsed
            .html_blocks
            .get(&open_start)
            .and_then(|block| block.details_span.clone())
            .expect("a disclosure inside a list item should still be linked");

        let hidden = parsed
            .events
            .get(span.hidden_events)
            .expect("the hidden events should be inside the event list");
        let hidden_text = |needle: &str| {
            hidden.iter().any(|(range, event)| {
                matches!(event, MarkdownEvent::Text) && source.get(range.clone()) == Some(needle)
            })
        };
        assert!(hidden_text("\u{672c}\u{6587}"), "{hidden:?}");
        assert!(!hidden_text("\u{5f8c}\u{308d}"), "{hidden:?}");

        let starts = hidden
            .iter()
            .filter(|(_, event)| matches!(event, MarkdownEvent::Start(_)))
            .count();
        let ends = hidden
            .iter()
            .filter(|(_, event)| matches!(event, MarkdownEvent::End(_)))
            .count();
        assert_eq!(
            starts, ends,
            "the hidden run has to be balanced or the div stack breaks: {hidden:?}"
        );
    }

    #[test]
    fn test_details_inside_a_block_quote_hides_only_its_own_body() {
        let source = "> <details>\n> <summary>\u{984c}</summary>\n>\n> \u{672c}\u{6587}\n>\n> </details>\n\n\u{5f8c}\u{308d}\n";
        let parsed = parse_markdown_with_options(source, true, false, false);

        let open_start = source.find("<details>").expect("the opening tag is there");
        let span = parsed
            .html_blocks
            .get(&open_start)
            .and_then(|block| block.details_span.clone())
            .expect("a disclosure inside a block quote should still be linked");

        let hidden = parsed
            .events
            .get(span.hidden_events)
            .expect("the hidden events should be inside the event list");
        let hidden_text = |needle: &str| {
            hidden.iter().any(|(range, event)| {
                matches!(event, MarkdownEvent::Text) && source.get(range.clone()) == Some(needle)
            })
        };
        assert!(hidden_text("\u{672c}\u{6587}"), "{hidden:?}");
        assert!(!hidden_text("\u{5f8c}\u{308d}"), "{hidden:?}");
    }

    #[test]
    fn test_details_body_events_survive_the_inline_html_rewrite() {
        // `hidden_events` is an index range, so anything that adds or drops an
        // event after it is measured would hide the wrong run. The inline
        // formatting tags before, inside and after the disclosure all go through
        // that rewrite, in a list item where the body cannot be cut at a root
        // block boundary.
        let source = "a <b>\u{592a}\u{5b57}</b>\n\n- item\n\n  <details>\n  <summary>\u{984c}</summary>\n\n  \u{672c}\u{6587} <u>\u{4e0b}\u{7dda}</u>\n\n  </details>\n\n\u{5f8c}\u{308d} <i>\u{659c}\u{4f53}</i>\n";
        let parsed = parse_markdown_with_options(source, true, false, false);

        let open_start = source.find("<details>").expect("the opening tag is there");
        let close_start = source.find("</details>").expect("the closing tag is there");
        let span = parsed
            .html_blocks
            .get(&open_start)
            .and_then(|block| block.details_span.clone())
            .expect("the disclosure should be linked to its closing block");
        let hidden = parsed
            .events
            .get(span.hidden_events)
            .expect("the hidden events should be inside the event list");

        assert!(
            hidden
                .iter()
                .all(|(range, _)| range.start >= open_start && range.start < close_start),
            "only the disclosure's own body may be hidden: {hidden:?}"
        );
        let hidden_has = |wanted: &MarkdownEvent| hidden.iter().any(|(_, event)| event == wanted);
        assert!(
            hidden_has(&MarkdownEvent::InlineHtmlStyleStart(
                InlineHtmlStyle::Underline
            )),
            "the body's own `<u>` belongs to the hidden run: {hidden:?}"
        );
        assert!(
            !hidden_has(&MarkdownEvent::Start(MarkdownTag::Strong))
                && !hidden_has(&MarkdownEvent::Start(MarkdownTag::Emphasis)),
            "the paragraphs around the disclosure stay visible: {hidden:?}"
        );

        let balance = |wanted_start: fn(&MarkdownEvent) -> bool,
                       wanted_end: fn(&MarkdownEvent) -> bool| {
            hidden
                .iter()
                .filter(|(_, event)| wanted_start(event))
                .count() as isize
                - hidden.iter().filter(|(_, event)| wanted_end(event)).count() as isize
        };
        assert_eq!(
            balance(
                |event| matches!(event, MarkdownEvent::Start(_)),
                |event| matches!(event, MarkdownEvent::End(_))
            ),
            0,
            "an unbalanced run would leave the renderer's div stack short: {hidden:?}"
        );
        assert_eq!(
            balance(
                |event| matches!(event, MarkdownEvent::RootStart),
                |event| matches!(event, MarkdownEvent::RootEnd(_))
            ),
            0,
            "an unbalanced run would leave the renderer's div stack short: {hidden:?}"
        );
    }

    #[test]
    fn test_crossed_inline_html_tags_stay_properly_nested() {
        // Equal counts are not enough: `<b>` and `<i>` that cross would pair
        // into interleaved spans, and the renderer pops the style stack blindly.
        // `Strong` and `Strong`, `Heading(H1)` and `Heading`: the variant name is
        // the only spelling the two tag enums share.
        let variant_name = |tag: String| {
            tag.split(['(', ' ', '{'])
                .next()
                .unwrap_or_default()
                .to_string()
        };
        for source in [
            "a <b>x<i>y</b>z</i> b",
            "a <b>x\n\ny</b> b",
            "- <b>x\n- y</b>\n",
            "> <b>x\n\n<i>y</b> z</i>\n",
        ] {
            let parsed = parse_markdown_with_options(source, true, false, false);
            let mut stack = Vec::new();
            for (_, event) in parsed.events.iter() {
                match event {
                    MarkdownEvent::Start(tag) => stack.push(variant_name(format!("{tag:?}"))),
                    MarkdownEvent::End(tag) => {
                        let opened = stack.pop().expect("an End without a Start");
                        assert_eq!(
                            opened,
                            variant_name(format!("{tag:?}")),
                            "{source:?}: {opened} was closed by {tag:?}: {:?}",
                            parsed.events
                        );
                    }
                    _ => {}
                }
            }
            assert!(stack.is_empty(), "{source:?} left {stack:?} open");
        }
    }

    #[test]
    fn test_self_contained_details_has_no_span() {
        let parsed = parse_markdown_with_options(
            "<details><summary>題</summary>本文</details>",
            true,
            false,
            false,
        );

        assert!(
            parsed
                .html_blocks
                .get(&0)
                .is_some_and(|block| block.details_span.is_none())
        );
    }

    #[test]
    fn test_unrecognized_inline_html_preserved_as_inline_html() {
        for input in ["a<span>b</span>c", "a<em>b</em>c", "a<strong>b</strong>c"] {
            let parsed = parse_markdown_with_options(input, false, false, false);
            let has_inline_html = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml));
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            assert!(
                has_inline_html,
                "unrecognized inline HTML \"{input}\" should emit InlineHtml"
            );
            assert!(
                !has_hard_break,
                "unrecognized inline HTML \"{input}\" should not emit HardBreak"
            );
        }
    }
}
