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
    pub has_untagged_code_block: bool,
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
    let mut has_untagged_code_block = false;
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
                            has_untagged_code_block = true;
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
        link_html_details_spans(&mut html_blocks, &state.events);
        let table_layouts = link_html_table_spans(&mut html_blocks, &state.events, text);
        link_html_aligned_spans(&mut html_blocks, &state.events, text);
        place_html_table_parts(&mut html_blocks, table_layouts);
        // Numbered last, since aligning a block can parse it again, and the
        // number is what a disclosure's open state is stored under.
        html::html_parser::assign_details_ordinals(&mut html_blocks);
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
        has_untagged_code_block,
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

/// Pairs each `<div align>`, `<div style="text-align: …">` or `<center>` an
/// HTML block leaves open with the later block that closes it, and aligns the
/// HTML blocks in between the way the container asks; the markdown blocks in
/// between are aligned by the renderer from the recorded event spans.
///
/// Tags are matched the way a browser matches them, so `</div>` closes the
/// innermost open `<div>` together with everything opened inside it, even from
/// inside a list item or a block quote the `<div>` encloses, and the end of a
/// list item or a block quote closes whatever was opened inside it. A
/// container that is never closed at all aligns nothing instead of the rest of
/// the document.
fn link_html_aligned_spans(
    html_blocks: &mut BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    events: &[(Range<usize>, MarkdownEvent)],
    text: &str,
) {
    use html::html_parser::{HtmlAlignedSpan, HtmlContainerName, HtmlContainerTag};

    struct OpenContainer {
        name: HtmlContainerName,
        text_align: Option<gpui::TextAlign>,
        block_start: usize,
        body_start: usize,
        /// How deep the opening block sits in lists and block quotes, so the
        /// container can be closed when the one it was opened in ends.
        depth: usize,
    }

    let mut open_containers: Vec<OpenContainer> = Vec::new();
    let mut spans: Vec<(usize, HtmlAlignedSpan)> = Vec::new();
    let mut html_block_events = Vec::new();
    // The blocks that close containers an earlier block opened, with those
    // containers as (stack position, name, alignment).
    let mut closing_blocks: Vec<(
        Range<usize>,
        Vec<(usize, HtmlContainerName, Option<gpui::TextAlign>)>,
    )> = Vec::new();
    let mut depth = 0usize;

    for (index, (range, event)) in events.iter().enumerate() {
        match event {
            MarkdownEvent::Start(tag) => {
                if let MarkdownTag::HtmlBlock = tag
                    && let Some(block) = html_blocks.get(&range.start)
                {
                    html_block_events.push((index, range.start));
                    let mut closed_from_earlier_blocks = Vec::new();
                    for container_tag in &block.container_tags {
                        match *container_tag {
                            HtmlContainerTag::Open { name, text_align } => {
                                open_containers.push(OpenContainer {
                                    name,
                                    text_align,
                                    block_start: range.start,
                                    // A parsed HTML block's own events are
                                    // dropped, so its end follows its start.
                                    body_start: index + 2,
                                    depth,
                                });
                            }
                            HtmlContainerTag::Close(name) => {
                                // A cell bounds what its tags can close, as it
                                // bounds a browser's scope, so a `</div>` in it
                                // leaves a `<div>` opened before the table open.
                                let boundary = if name == HtmlContainerName::TableCell {
                                    0
                                } else {
                                    open_containers
                                        .iter()
                                        .rposition(|open| open.name == HtmlContainerName::TableCell)
                                        .map_or(0, |position| position + 1)
                                };
                                let Some(position) = open_containers
                                    .get(boundary..)
                                    .and_then(|above| {
                                        above.iter().rposition(|open| open.name == name)
                                    })
                                    .map(|offset| boundary + offset)
                                else {
                                    continue;
                                };
                                for (offset, open) in open_containers.drain(position..).enumerate()
                                {
                                    if open.block_start != range.start {
                                        closed_from_earlier_blocks.push((
                                            position + offset,
                                            open.name,
                                            open.text_align,
                                        ));
                                    }
                                    // One opened and closed in this very block
                                    // was already aligned by the HTML parser.
                                    if let Some(text_align) = open.text_align
                                        && open.body_start < index
                                    {
                                        spans.push((
                                            open.block_start,
                                            HtmlAlignedSpan {
                                                events: open.body_start..index,
                                                text_align,
                                                nesting: position + offset,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    if !closed_from_earlier_blocks.is_empty() {
                        closing_blocks.push((range.clone(), closed_from_earlier_blocks));
                    }
                }
                depth += 1;
            }
            MarkdownEvent::End(_) => {
                depth = depth.saturating_sub(1);
                // Containers are opened no shallower than the ones before
                // them, so the ones this end closes are at the top.
                while let Some(open) = open_containers.pop_if(|open| open.depth > depth) {
                    if let Some(text_align) = open.text_align
                        && open.body_start < index
                    {
                        spans.push((
                            open.block_start,
                            HtmlAlignedSpan {
                                events: open.body_start..index,
                                text_align,
                                nesting: open_containers.len(),
                            },
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // Whatever a closing block holds before its `</div>` still sits inside the
    // container, the way the rest of an opening block after its `<div align>`
    // does. The tree builder only knows where that part ends if it sees the
    // opening tags, so the block is parsed again behind them; the part after
    // the close comes out as it did before. A `<details>` among the containers
    // has to be opened as one too, or its `</details>` would close nothing, and
    // is then taken out again, leaving what it held in its place.
    for (block_range, mut containers) in closing_blocks {
        if containers
            .iter()
            .all(|(_, _, text_align)| text_align.is_none())
        {
            continue;
        }
        let Some(block) = html_blocks.get_mut(&block_range.start) else {
            continue;
        };
        let Some(source) = text.get(block_range.clone()) else {
            continue;
        };

        containers.sort_by_key(|(position, _, _)| *position);
        let mut enclosed_source = String::new();
        let mut enclosing_details = 0;
        for (_, name, text_align) in containers {
            let tag_name = match name {
                HtmlContainerName::Center => "center",
                HtmlContainerName::Div | HtmlContainerName::TableCell => "div",
                HtmlContainerName::Details => {
                    enclosing_details += 1;
                    "details"
                }
            };
            enclosed_source.push('<');
            enclosed_source.push_str(tag_name);
            if let Some(text_align) = text_align {
                enclosed_source.push_str(match text_align {
                    gpui::TextAlign::Left => " style=\"text-align: left\"",
                    gpui::TextAlign::Center => " style=\"text-align: center\"",
                    gpui::TextAlign::Right => " style=\"text-align: right\"",
                });
            }
            enclosed_source.push('>');
            // The stand-in disclosure gets a summary of its own, or it would
            // take a `<summary>` the block itself holds and drop its text.
            if name == HtmlContainerName::Details {
                enclosed_source.push_str("<summary>-</summary>");
            }
        }
        enclosed_source.push_str(source);
        if let Some(enclosed) =
            html::html_parser::parse_html_block(&enclosed_source, block_range.clone())
        {
            block.children = without_enclosing_details(enclosed.children, enclosing_details);
        }
    }

    if spans.is_empty() {
        return;
    }

    for (block_start, span) in &spans {
        if let Some(block) = html_blocks.get_mut(block_start) {
            block.aligned_spans.push(span.clone());
        }
    }

    spans.sort_by_key(|(_, span)| span.events.start);
    let mut pending_spans = spans.iter().map(|(_, span)| span).peekable();
    let mut active_spans: Vec<&HtmlAlignedSpan> = Vec::new();
    for (index, block_start) in html_block_events {
        while let Some(span) = pending_spans.next_if(|span| span.events.start <= index) {
            active_spans.push(span);
        }
        active_spans.retain(|span| span.events.end > index);
        let Some(innermost) = active_spans.iter().max_by_key(|span| span.nesting) else {
            continue;
        };
        if let Some(block) = html_blocks.get_mut(&block_start) {
            html::html_parser::inherit_text_align(&mut block.children, innermost.text_align);
        }
    }
}

/// Takes out the `count` disclosures a closing block was parsed behind. They
/// were opened in front of everything else, so each is the first element of
/// the one before, and what follows its `</details>` comes after it.
fn without_enclosing_details(
    elements: Vec<html::html_parser::ParsedHtmlElement>,
    count: usize,
) -> Vec<html::html_parser::ParsedHtmlElement> {
    use html::html_parser::ParsedHtmlElement;

    let mut following = Vec::new();
    let mut elements = elements;
    for _ in 0..count {
        if !matches!(elements.first(), Some(ParsedHtmlElement::Details(_))) {
            break;
        }
        let ParsedHtmlElement::Details(details) = elements.remove(0) else {
            break;
        };
        following.push(std::mem::replace(&mut elements, details.children));
    }
    for rest in following.into_iter().rev() {
        elements.extend(rest);
    }
    elements
}

/// A `<table>` as the walk over the events finds it.
struct HtmlTableRecord {
    source_start: usize,
    /// The table's own `align`, or what encloses it in the same block.
    text_align: Option<gpui::TextAlign>,
    closed: bool,
    /// Set when the table cannot be drawn with its cells spanning blocks:
    /// markdown outside a cell, a tag at another depth, a stray end.
    broken: bool,
    /// The blocks holding the table's own tags, in order.
    block_starts: Vec<usize>,
    row_count: usize,
    row_has_cells: bool,
    current_row: usize,
    next_column: usize,
    column_count: usize,
    column_widths: Vec<Option<html::html_parser::HtmlColumnWidth>>,
    /// For each column, the first row a `rowspan` from above no longer covers.
    covered_until: Vec<usize>,
    caption: Option<HtmlTableCaption>,
}

struct HtmlTableCaption {
    /// What the `<caption>` holds, between its tags, in the document.
    range: Range<usize>,
    below: bool,
    text_align: Option<gpui::TextAlign>,
}

#[derive(Clone)]
enum HtmlLayoutEntryKind {
    Table,
    Section,
    Row {
        text_align: Option<gpui::TextAlign>,
        vertical_align: Option<html::html_parser::HtmlVerticalAlign>,
    },
    Cell {
        text_align: Option<gpui::TextAlign>,
    },
    Caption {
        /// Where the caption's content starts in the document.
        start: usize,
        below: bool,
        text_align: Option<gpui::TextAlign>,
    },
    Container {
        name: html::html_parser::HtmlContainerName,
        text_align: Option<gpui::TextAlign>,
    },
}

#[derive(Clone)]
struct HtmlLayoutEntry {
    kind: HtmlLayoutEntryKind,
    /// The table a table, section, row or cell belongs to.
    table: Option<usize>,
    depth: usize,
}

enum HtmlTableOp {
    OpenTable,
    OpenCell(html::html_parser::HtmlSpanningCell),
    CloseCell,
    EndRow {
        row: usize,
        occupied_columns: Vec<bool>,
    },
    CloseTable,
}

/// A table tag of a block, which cuts the block when its table spans blocks.
struct HtmlTableSplit {
    range: Range<usize>,
    table: usize,
    /// What the tag does, closing first whatever it closes by implication.
    ops: Vec<HtmlTableOp>,
    /// What is open right after the tag, which is what the HTML up to the next
    /// cut sits in.
    stack_after: Vec<HtmlLayoutEntry>,
}

enum HtmlTableBlockItem {
    Container(html::html_parser::HtmlContainerTag),
    Split(HtmlTableSplit),
}

struct ScannedHtmlTableBlock {
    block_start: usize,
    event_index: usize,
    stack_before: Vec<HtmlLayoutEntry>,
    items: Vec<HtmlTableBlockItem>,
}

/// How a block taking part in a table that spans blocks is drawn, worked out
/// before the aligned containers are paired and put into the block after,
/// since pairing them may parse a closing block's HTML again.
struct HtmlTableBlockLayout {
    block_start: usize,
    event_index: usize,
    children: Vec<html::html_parser::ParsedHtmlElement>,
    parts: Vec<html::html_parser::HtmlTablePart>,
    /// The children nothing in the table's blocks aligns, which take the
    /// alignment of a container an earlier block opened.
    unaligned: Vec<Range<usize>>,
}

/// Pairs each `<table>` with its rows and cells across blocks. README layouts
/// leave blank lines inside `<td>` so its body is parsed as markdown, which
/// ends the CommonMark HTML block right after the opening tags, so the table,
/// its markdown body and `</td></tr></table>` arrive as separate blocks.
///
/// Tags are matched the way a browser matches them: `<td>` closes the cell
/// before it, `</tr>` closes its cell, `</table>` everything inside it, and
/// `</td>` the `<div>`s opened in the cell. A table that is never closed, that
/// has markdown outside its cells, or whose tags end up at another list or
/// block quote depth is left as it was drawn before, rather than risk losing
/// what does not fit. The cells of the tables that pair are added to the
/// blocks' container tags, so the aligned container pairing aligns their
/// markdown bodies the way it aligns a `<div>`'s.
fn link_html_table_spans(
    html_blocks: &mut BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    events: &[(Range<usize>, MarkdownEvent)],
    text: &str,
) -> Vec<HtmlTableBlockLayout> {
    use html::html_parser::{
        HtmlContainerName, HtmlContainerTag, HtmlLayoutTagKind, HtmlSpanningTable, HtmlTablePart,
        layout_tags,
    };

    let mut stack: Vec<HtmlLayoutEntry> = Vec::new();
    let mut tables: Vec<HtmlTableRecord> = Vec::new();
    let mut scanned_blocks: Vec<ScannedHtmlTableBlock> = Vec::new();
    let mut depth = 0usize;

    for (index, (range, event)) in events.iter().enumerate() {
        match event {
            MarkdownEvent::Start(MarkdownTag::HtmlBlock)
                if html_blocks.contains_key(&range.start) =>
            {
                if let Some(source) = text.get(range.clone())
                    && (!stack.is_empty() || may_hold_table_tags(source))
                {
                    let stack_before = stack.clone();
                    let mut items = Vec::new();
                    let mut gap_start = 0;
                    for tag in layout_tags(source) {
                        mark_html_outside_cells(
                            &stack,
                            source.get(gap_start..tag.range.start).unwrap_or_default(),
                            &mut tables,
                        );
                        gap_start = tag.range.end;
                        match tag.kind {
                            HtmlLayoutTagKind::Container(container_tag) => {
                                apply_html_container_tag(&mut stack, container_tag, depth);
                                items.push(HtmlTableBlockItem::Container(container_tag));
                            }
                            kind => {
                                let Some((table, ops)) = apply_html_table_tag(
                                    &mut stack,
                                    &mut tables,
                                    kind,
                                    depth,
                                    range.start + tag.range.start..range.start + tag.range.end,
                                ) else {
                                    continue;
                                };
                                if let Some(record) = tables.get_mut(table)
                                    && record.block_starts.last() != Some(&range.start)
                                {
                                    record.block_starts.push(range.start);
                                }
                                items.push(HtmlTableBlockItem::Split(HtmlTableSplit {
                                    range: tag.range,
                                    table,
                                    ops,
                                    stack_after: stack.clone(),
                                }));
                            }
                        }
                    }
                    mark_html_outside_cells(
                        &stack,
                        source.get(gap_start..).unwrap_or_default(),
                        &mut tables,
                    );
                    // A caption is drawn from its block's source, so one whose
                    // content goes on into the next block cannot be.
                    for entry in &stack {
                        if matches!(entry.kind, HtmlLayoutEntryKind::Caption { .. })
                            && let Some(record) =
                                entry.table.and_then(|table| tables.get_mut(table))
                        {
                            record.broken = true;
                        }
                    }
                    if items
                        .iter()
                        .any(|item| matches!(item, HtmlTableBlockItem::Split(_)))
                    {
                        scanned_blocks.push(ScannedHtmlTableBlock {
                            block_start: range.start,
                            event_index: index,
                            stack_before,
                            items,
                        });
                    }
                    // Containers outside every table are the aligned container
                    // pairing's to follow.
                    if !stack
                        .iter()
                        .any(|entry| matches!(entry.kind, HtmlLayoutEntryKind::Table))
                    {
                        stack.clear();
                    }
                }
                depth += 1;
            }
            MarkdownEvent::Start(_) => {
                mark_markdown_outside_cells(&stack, &mut tables);
                depth += 1;
            }
            MarkdownEvent::Rule => mark_markdown_outside_cells(&stack, &mut tables),
            MarkdownEvent::End(_) => {
                depth = depth.saturating_sub(1);
                while stack.last().is_some_and(|entry| entry.depth > depth) {
                    if let Some(entry) = stack.pop()
                        && matches!(entry.kind, HtmlLayoutEntryKind::Table)
                        && let Some(record) = entry.table.and_then(|table| tables.get_mut(table))
                    {
                        record.broken = true;
                    }
                }
            }
            _ => {}
        }
    }

    let mut valid = tables
        .iter()
        .map(|record| {
            record.closed
                && !record.broken
                && record.block_starts.len() > 1
                && record.column_count > 0
        })
        .collect::<Vec<_>>();
    if !valid.contains(&true) {
        return Vec::new();
    }

    // Collapsing a disclosure skips its body's events. One whose body holds
    // some of a table's blocks but not all of them would skip the tags that
    // close what the others open.
    let event_indices = scanned_blocks
        .iter()
        .map(|block| (block.block_start, block.event_index))
        .collect::<HashMap<_, _>>();
    let hidden_ranges = html_blocks
        .values()
        .filter_map(|block| {
            block
                .details_span
                .as_ref()
                .map(|span| span.hidden_events.clone())
        })
        .collect::<Vec<_>>();
    loop {
        let mut changed = false;
        for (table, record) in tables.iter().enumerate() {
            if !valid.get(table).copied().unwrap_or(false) {
                continue;
            }
            let block_events = record
                .block_starts
                .iter()
                .filter_map(|block_start| event_indices.get(block_start))
                .collect::<Vec<_>>();
            let crosses = hidden_ranges.iter().any(|hidden| {
                let inside = block_events
                    .iter()
                    .filter(|index| hidden.contains(**index))
                    .count();
                inside > 0 && inside < block_events.len()
            });
            if crosses && let Some(valid) = valid.get_mut(table) {
                *valid = false;
                changed = true;
            }
        }
        // A disclosure whose body continues in later blocks has to be the last
        // thing its block opens, or its body would reach past the cell.
        for block in &scanned_blocks {
            let Some(html_block) = html_blocks.get(&block.block_start) else {
                continue;
            };
            if html_block.details_span.is_none() {
                continue;
            }
            let Some(last_split_index) = block.items.iter().rposition(|item| {
                matches!(
                    item,
                    HtmlTableBlockItem::Split(split)
                        if valid.get(split.table).copied().unwrap_or(false)
                )
            }) else {
                continue;
            };
            let Some(HtmlTableBlockItem::Split(last_split)) = block.items.get(last_split_index)
            else {
                continue;
            };
            let mut details_left_open = 0usize;
            for item in block.items.iter().skip(last_split_index + 1) {
                match item {
                    HtmlTableBlockItem::Container(HtmlContainerTag::Open {
                        name: HtmlContainerName::Details,
                        ..
                    }) => details_left_open += 1,
                    HtmlTableBlockItem::Container(HtmlContainerTag::Close(
                        HtmlContainerName::Details,
                    )) => details_left_open = details_left_open.saturating_sub(1),
                    _ => {}
                }
            }
            if details_left_open == 0
                && let Some(valid) = valid.get_mut(last_split.table)
            {
                *valid = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let is_valid = |table: usize| valid.get(table).copied().unwrap_or(false);
    let mut layouts = Vec::new();
    for block in scanned_blocks {
        if !block
            .items
            .iter()
            .any(|item| matches!(item, HtmlTableBlockItem::Split(split) if is_valid(split.table)))
        {
            continue;
        }
        let Some(html_block) = html_blocks.get_mut(&block.block_start) else {
            continue;
        };
        let source_range = html_block.source_range.clone();
        let Some(source) = text.get(source_range.clone()) else {
            continue;
        };

        let mut container_tags = Vec::new();
        let mut segments = HtmlTableSegments::default();
        let mut segment_start = 0;
        let mut context = block.stack_before;
        for item in block.items {
            let split = match item {
                HtmlTableBlockItem::Container(tag) => {
                    container_tags.push(tag);
                    continue;
                }
                HtmlTableBlockItem::Split(split) if is_valid(split.table) => split,
                HtmlTableBlockItem::Split(_) => continue,
            };
            segments.push(
                source,
                source_range.start,
                segment_start..split.range.start,
                &context,
                &valid,
            );
            for op in split.ops {
                let part = match op {
                    HtmlTableOp::OpenTable => {
                        let Some(record) = tables.get(split.table) else {
                            continue;
                        };
                        let mut column_widths = record.column_widths.clone();
                        column_widths.resize(record.column_count, None);
                        HtmlTablePart::OpenTable(HtmlSpanningTable {
                            source_start: record.source_start,
                            column_count: record.column_count,
                            text_align: record.text_align,
                            column_widths,
                            has_caption: record.caption.is_some(),
                            caption_above: record
                                .caption
                                .as_ref()
                                .filter(|caption| !caption.below)
                                .and_then(|caption| segments.push_caption(text, caption)),
                        })
                    }
                    HtmlTableOp::OpenCell(mut cell) => {
                        // A `rowspan` past the last row would add rows of its
                        // own below the table.
                        let row_count =
                            tables.get(split.table).map_or(1, |record| record.row_count);
                        cell.row_span =
                            cell.row_span.min(row_count.saturating_sub(cell.row)).max(1);
                        container_tags.push(HtmlContainerTag::Open {
                            name: HtmlContainerName::TableCell,
                            text_align: cell.text_align,
                        });
                        HtmlTablePart::OpenCell(cell)
                    }
                    HtmlTableOp::CloseCell => {
                        container_tags.push(HtmlContainerTag::Close(HtmlContainerName::TableCell));
                        HtmlTablePart::CloseCell
                    }
                    HtmlTableOp::EndRow {
                        row,
                        occupied_columns,
                    } => HtmlTablePart::EndRow {
                        row,
                        occupied_columns,
                    },
                    HtmlTableOp::CloseTable => {
                        let caption = tables
                            .get(split.table)
                            .and_then(|record| record.caption.as_ref());
                        HtmlTablePart::CloseTable {
                            has_caption: caption.is_some(),
                            caption_below: caption
                                .filter(|caption| caption.below)
                                .and_then(|caption| segments.push_caption(text, caption)),
                        }
                    }
                };
                segments.parts.push(part);
            }
            segment_start = split.range.end;
            context = split.stack_after;
        }
        segments.push(
            source,
            source_range.start,
            segment_start..source.len(),
            &context,
            &valid,
        );

        html_block.container_tags = container_tags;
        layouts.push(HtmlTableBlockLayout {
            block_start: block.block_start,
            event_index: block.event_index,
            children: segments.children,
            parts: segments.parts,
            unaligned: segments.unaligned,
        });
    }
    layouts
}

/// Gives the blocks taking part in a table that spans blocks the parts they
/// are drawn with, once the aligned containers are paired, and aligns their
/// HTML that nothing in the table's own blocks aligns by the innermost
/// container an earlier block opened around it.
fn place_html_table_parts(
    html_blocks: &mut BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    layouts: Vec<HtmlTableBlockLayout>,
) {
    if layouts.is_empty() {
        return;
    }

    let spans = html_blocks
        .values()
        .flat_map(|block| block.aligned_spans.iter().cloned())
        .collect::<Vec<_>>();
    for mut layout in layouts {
        let outer_text_align = spans
            .iter()
            .filter(|span| span.events.contains(&layout.event_index))
            .max_by_key(|span| span.nesting)
            .map(|span| span.text_align);
        if let Some(text_align) = outer_text_align {
            for range in &layout.unaligned {
                if let Some(elements) = layout.children.get_mut(range.clone()) {
                    html::html_parser::inherit_text_align(elements, text_align);
                }
            }
        }
        if let Some(block) = html_blocks.get_mut(&layout.block_start) {
            block.children = layout.children;
            block.table_parts = layout.parts;
        }
    }
}

#[derive(Default)]
struct HtmlTableSegments {
    children: Vec<html::html_parser::ParsedHtmlElement>,
    parts: Vec<html::html_parser::HtmlTablePart>,
    unaligned: Vec<Range<usize>>,
}

impl HtmlTableSegments {
    /// Parses the HTML of a block between two of its table tags on its own,
    /// giving it the alignment of whatever it sits in.
    fn push(
        &mut self,
        source: &str,
        block_offset: usize,
        segment: Range<usize>,
        context: &[HtmlLayoutEntry],
        valid: &[bool],
    ) {
        let Some(fragment) = source.get(segment.clone()) else {
            return;
        };
        if fragment.trim().is_empty() {
            return;
        }
        // Between a table's rows and cells the walk only let through what
        // draws nothing, such as a comment.
        let innermost = context.iter().rev().find(|entry| {
            entry
                .table
                .is_some_and(|table| valid.get(table).copied().unwrap_or(false))
        });
        if innermost.is_some_and(|entry| !matches!(entry.kind, HtmlLayoutEntryKind::Cell { .. })) {
            return;
        }
        let Some(parsed) = html::html_parser::parse_html_block(
            fragment,
            block_offset + segment.start..block_offset + segment.end,
        ) else {
            return;
        };
        if parsed.children.is_empty() {
            return;
        }

        let start = self.children.len();
        let mut elements = parsed.children;
        match html_layout_text_align(context, Some(valid)) {
            Some(text_align) => html::html_parser::inherit_text_align(&mut elements, text_align),
            None => self.unaligned.push(start..start + elements.len()),
        }
        self.children.extend(elements);
        self.parts.push(html::html_parser::HtmlTablePart::Elements(
            start..self.children.len(),
        ));
    }

    /// Parses a table's `<caption>` into the block's children, centered the
    /// way a browser centers it, and returns where it landed.
    fn push_caption(&mut self, text: &str, caption: &HtmlTableCaption) -> Option<Range<usize>> {
        let fragment = text.get(caption.range.clone())?;
        if fragment.trim().is_empty() {
            return None;
        }
        let parsed = html::html_parser::parse_html_block(fragment, caption.range.clone())?;
        let mut elements = parsed.children;
        if elements.is_empty() {
            return None;
        }
        html::html_parser::inherit_text_align(
            &mut elements,
            caption.text_align.unwrap_or(gpui::TextAlign::Center),
        );
        let start = self.children.len();
        self.children.extend(elements);
        Some(start..self.children.len())
    }
}

/// Whether a block may hold a table's tags, to spare reading every other
/// block's tags while no table is open.
fn may_hold_table_tags(source: &str) -> bool {
    let lowered = source.to_ascii_lowercase();
    lowered.contains("<t") || lowered.contains("</t")
}

/// The alignment the innermost of `stack` asks for, skipping the tables that
/// did not pair when `valid` says which did.
fn html_layout_text_align(
    stack: &[HtmlLayoutEntry],
    valid: Option<&[bool]>,
) -> Option<gpui::TextAlign> {
    stack
        .iter()
        .rev()
        .filter(|entry| {
            entry.table.is_none_or(|table| {
                valid.is_none_or(|valid| valid.get(table).copied().unwrap_or(false))
            })
        })
        .find_map(|entry| match entry.kind {
            HtmlLayoutEntryKind::Container { text_align, .. }
            | HtmlLayoutEntryKind::Cell { text_align }
            | HtmlLayoutEntryKind::Row { text_align, .. } => text_align,
            HtmlLayoutEntryKind::Table
            | HtmlLayoutEntryKind::Section
            | HtmlLayoutEntryKind::Caption { .. } => None,
        })
}

/// Markdown can only be drawn inside a cell; anywhere else in a table there is
/// no place for it that would not lose it, so the table is drawn as before.
fn mark_markdown_outside_cells(stack: &[HtmlLayoutEntry], tables: &mut [HtmlTableRecord]) {
    if let Some(entry) = stack.iter().rev().find(|entry| entry.table.is_some())
        && !matches!(entry.kind, HtmlLayoutEntryKind::Cell { .. })
        && let Some(record) = entry.table.and_then(|table| tables.get_mut(table))
    {
        record.broken = true;
    }
}

fn mark_html_outside_cells(
    stack: &[HtmlLayoutEntry],
    fragment: &str,
    tables: &mut [HtmlTableRecord],
) {
    let outside_cells = stack
        .iter()
        .rev()
        .find(|entry| entry.table.is_some())
        .is_some_and(|entry| {
            !matches!(
                entry.kind,
                HtmlLayoutEntryKind::Cell { .. } | HtmlLayoutEntryKind::Caption { .. }
            )
        });
    if outside_cells
        && !fragment.trim().is_empty()
        && html::html_parser::parse_html_block(fragment, 0..fragment.len())
            .is_some_and(|block| !block.children.is_empty())
    {
        mark_markdown_outside_cells(stack, tables);
    }
}

fn apply_html_container_tag(
    stack: &mut Vec<HtmlLayoutEntry>,
    tag: html::html_parser::HtmlContainerTag,
    depth: usize,
) {
    use html::html_parser::HtmlContainerTag;

    match tag {
        HtmlContainerTag::Open { name, text_align } => stack.push(HtmlLayoutEntry {
            kind: HtmlLayoutEntryKind::Container { name, text_align },
            table: None,
            depth,
        }),
        HtmlContainerTag::Close(name) => {
            // A `</div>` does not reach past the cell or the table it is in.
            let boundary = stack
                .iter()
                .rposition(|entry| entry.table.is_some())
                .map_or(0, |position| position + 1);
            let Some(offset) = stack.get(boundary..).and_then(|above| {
                above.iter().rposition(|entry| {
                    matches!(
                        entry.kind,
                        HtmlLayoutEntryKind::Container { name: open, .. } if open == name
                    )
                })
            }) else {
                return;
            };
            let position = boundary + offset;
            if stack
                .get(position)
                .is_some_and(|entry| entry.depth == depth)
            {
                stack.truncate(position);
            }
        }
    }
}

/// Applies one table tag to the stack of what is open, returning the table it
/// belongs to and what it does, or `None` for a tag no open table can take.
fn apply_html_table_tag(
    stack: &mut Vec<HtmlLayoutEntry>,
    tables: &mut Vec<HtmlTableRecord>,
    kind: html::html_parser::HtmlLayoutTagKind,
    depth: usize,
    tag_range: Range<usize>,
) -> Option<(usize, Vec<HtmlTableOp>)> {
    use html::html_parser::{HtmlLayoutTagKind, HtmlSpanningCell, MAX_SPANNING_TABLE_COLUMNS};

    let mut ops = Vec::new();

    if let HtmlLayoutTagKind::OpenTable { text_align } = kind {
        // A table straight inside a row has nowhere to go but out of the
        // enclosing table, which a browser moves it in front of.
        mark_markdown_outside_cells(stack, tables);
        let table = tables.len();
        tables.push(HtmlTableRecord {
            source_start: tag_range.start,
            text_align: text_align.or_else(|| html_layout_text_align(stack, None)),
            closed: false,
            broken: false,
            block_starts: Vec::new(),
            row_count: 0,
            row_has_cells: false,
            current_row: 0,
            next_column: 0,
            column_count: 0,
            column_widths: Vec::new(),
            covered_until: Vec::new(),
            caption: None,
        });
        stack.push(HtmlLayoutEntry {
            kind: HtmlLayoutEntryKind::Table,
            table: Some(table),
            depth,
        });
        ops.push(HtmlTableOp::OpenTable);
        return Some((table, ops));
    }

    let table_position = stack
        .iter()
        .rposition(|entry| matches!(entry.kind, HtmlLayoutEntryKind::Table))?;
    let table_entry = stack.get(table_position)?;
    let table = table_entry.table?;
    let table_depth = table_entry.depth;
    let record = tables.get_mut(table)?;
    if table_depth != depth {
        record.broken = true;
        return None;
    }

    let above_table = table_position + 1;
    let find_above = |stack: &[HtmlLayoutEntry], wanted: fn(&HtmlLayoutEntryKind) -> bool| {
        stack
            .get(above_table..)
            .and_then(|above| above.iter().rposition(|entry| wanted(&entry.kind)))
            .map(|offset| above_table + offset)
    };
    // Where a new row goes: right above the innermost section, or the table.
    let row_anchor = find_above(stack, |kind| matches!(kind, HtmlLayoutEntryKind::Section))
        .map_or(above_table, |position| position + 1);

    match kind {
        HtmlLayoutTagKind::CloseTable => {
            pop_html_table_entries(stack, table_position, record, &mut ops, tag_range.start);
        }
        HtmlLayoutTagKind::OpenSection => {
            pop_html_table_entries(stack, above_table, record, &mut ops, tag_range.start);
            stack.push(HtmlLayoutEntry {
                kind: HtmlLayoutEntryKind::Section,
                table: Some(table),
                depth,
            });
        }
        HtmlLayoutTagKind::CloseSection => {
            if let Some(position) =
                find_above(stack, |kind| matches!(kind, HtmlLayoutEntryKind::Section))
            {
                pop_html_table_entries(stack, position, record, &mut ops, tag_range.start);
            }
        }
        HtmlLayoutTagKind::OpenRow {
            text_align,
            vertical_align,
        } => {
            pop_html_table_entries(stack, row_anchor, record, &mut ops, tag_range.start);
            stack.push(HtmlLayoutEntry {
                kind: HtmlLayoutEntryKind::Row {
                    text_align,
                    vertical_align,
                },
                table: Some(table),
                depth,
            });
        }
        HtmlLayoutTagKind::CloseRow => {
            if let Some(position) = find_above(stack, |kind| {
                matches!(kind, HtmlLayoutEntryKind::Row { .. })
            }) {
                pop_html_table_entries(stack, position, record, &mut ops, tag_range.start);
            }
        }
        HtmlLayoutTagKind::OpenCell {
            is_header,
            text_align,
            vertical_align,
            col_span,
            row_span,
            width,
        } => {
            let row_position = match find_above(stack, |kind| {
                matches!(kind, HtmlLayoutEntryKind::Row { .. })
            }) {
                Some(position) => {
                    pop_html_table_entries(stack, position + 1, record, &mut ops, tag_range.start);
                    position
                }
                // A cell with no row around it starts one, as in a browser.
                None => {
                    pop_html_table_entries(stack, row_anchor, record, &mut ops, tag_range.start);
                    stack.push(HtmlLayoutEntry {
                        kind: HtmlLayoutEntryKind::Row {
                            text_align: None,
                            vertical_align: None,
                        },
                        table: Some(table),
                        depth,
                    });
                    stack.len() - 1
                }
            };
            let (row_text_align, row_vertical_align) = match stack.get(row_position) {
                Some(HtmlLayoutEntry {
                    kind:
                        HtmlLayoutEntryKind::Row {
                            text_align,
                            vertical_align,
                        },
                    ..
                }) => (*text_align, *vertical_align),
                _ => (None, None),
            };

            if !record.row_has_cells {
                record.current_row = record.row_count;
                record.row_count += 1;
                record.next_column = 0;
                record.row_has_cells = true;
            }
            // The columns a `rowspan` from an earlier row still covers are
            // skipped, the way a browser places the cell after them.
            let mut column = record.next_column;
            while record
                .covered_until
                .get(column)
                .is_some_and(|until| *until > record.current_row)
            {
                column += 1;
            }
            if column >= MAX_SPANNING_TABLE_COLUMNS {
                record.broken = true;
                return None;
            }
            let col_span = col_span.min(MAX_SPANNING_TABLE_COLUMNS - column);
            record.next_column = column + col_span;
            record.column_count = record.column_count.max(record.next_column);
            if record.covered_until.len() < record.next_column {
                record.covered_until.resize(record.next_column, 0);
            }
            for until in record
                .covered_until
                .get_mut(column..record.next_column)
                .into_iter()
                .flatten()
            {
                *until = record.current_row + row_span;
            }
            if col_span == 1
                && let Some(width) = width
            {
                if record.column_widths.len() <= column {
                    record.column_widths.resize(column + 1, None);
                }
                if let Some(column_width) = record.column_widths.get_mut(column) {
                    *column_width = Some(html::html_parser::HtmlColumnWidth::merge(
                        *column_width,
                        width,
                    ));
                }
            }

            let text_align = text_align
                .or(row_text_align)
                .or(is_header.then_some(gpui::TextAlign::Center));
            stack.push(HtmlLayoutEntry {
                kind: HtmlLayoutEntryKind::Cell { text_align },
                table: Some(table),
                depth,
            });
            ops.push(HtmlTableOp::OpenCell(HtmlSpanningCell {
                row: record.current_row,
                column,
                col_span,
                row_span,
                is_header,
                text_align,
                vertical_align: vertical_align.or(row_vertical_align).unwrap_or_default(),
            }));
        }
        HtmlLayoutTagKind::CloseCell => {
            if let Some(position) = find_above(stack, |kind| {
                matches!(kind, HtmlLayoutEntryKind::Cell { .. })
            }) {
                pop_html_table_entries(stack, position, record, &mut ops, tag_range.start);
            }
        }
        HtmlLayoutTagKind::OpenCaption { below, text_align } => {
            pop_html_table_entries(stack, above_table, record, &mut ops, tag_range.start);
            if record.caption.is_some() {
                // Only one caption has a place beside the table.
                record.broken = true;
                return None;
            }
            stack.push(HtmlLayoutEntry {
                kind: HtmlLayoutEntryKind::Caption {
                    start: tag_range.end,
                    below,
                    text_align,
                },
                table: Some(table),
                depth,
            });
        }
        HtmlLayoutTagKind::CloseCaption => {
            if let Some(position) = find_above(stack, |kind| {
                matches!(kind, HtmlLayoutEntryKind::Caption { .. })
            }) {
                pop_html_table_entries(stack, position, record, &mut ops, tag_range.start);
            }
        }
        HtmlLayoutTagKind::OpenTable { .. } | HtmlLayoutTagKind::Container(_) => {}
    }

    Some((table, ops))
}

/// Closes everything of the innermost table above `keep`, the way the tag
/// that closes an outer part of the table closes its inner parts. `at` is
/// where the closing tag starts in the document.
fn pop_html_table_entries(
    stack: &mut Vec<HtmlLayoutEntry>,
    keep: usize,
    record: &mut HtmlTableRecord,
    ops: &mut Vec<HtmlTableOp>,
    at: usize,
) {
    while stack.len() > keep {
        let Some(entry) = stack.pop() else {
            break;
        };
        match entry.kind {
            HtmlLayoutEntryKind::Cell { .. } => ops.push(HtmlTableOp::CloseCell),
            HtmlLayoutEntryKind::Row { .. } => {
                if record.row_has_cells {
                    let current_row = record.current_row;
                    ops.push(HtmlTableOp::EndRow {
                        row: current_row,
                        occupied_columns: record
                            .covered_until
                            .iter()
                            .map(|until| *until > current_row)
                            .collect(),
                    });
                }
                record.row_has_cells = false;
            }
            HtmlLayoutEntryKind::Caption {
                start,
                below,
                text_align,
            } => {
                record.caption = Some(HtmlTableCaption {
                    range: start..at.max(start),
                    below,
                    text_align,
                });
            }
            HtmlLayoutEntryKind::Table => {
                ops.push(HtmlTableOp::CloseTable);
                record.closed = true;
            }
            HtmlLayoutEntryKind::Section | HtmlLayoutEntryKind::Container { .. } => {}
        }
    }
}

/// An inline HTML tag the preview can style instead of dropping.
struct InlineHtmlTag {
    name: &'static str,
    /// The destination an opening `<a>` links to.
    destination: Option<String>,
    /// What an opening `<span>`, `<mark>` or `<font>` asks to be drawn with.
    span_style: InlineSpanStyle,
    closing: bool,
}

/// The inline HTML tags a plain paragraph may carry that the preview can style
/// instead of dropping. `<b>text</b>` reaches the renderer as two empty
/// substitutions around the text, which used to leave the text unstyled.
///
/// An opening `<a>` also hands back the destination it links to, and an opening
/// `<span>`, `<mark>` or `<font>` the colors and size it asks for, since those
/// are the tags whose rewritten events need more than their name.
fn inline_html_style_tag(html: &str) -> Option<InlineHtmlTag> {
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
        // A `<span>` that asks for nothing is still recognized, so that its
        // `</span>` closes it rather than an enclosing colored one.
        "span" => "span",
        "mark" => "mark",
        "font" => "font",
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

    let span_style = if matches!(name, "span" | "mark" | "font") && !closing {
        let styles = inline_html_attribute(inner, "style")
            .map(|style| html::html_parser::parse_style_declarations(&style))
            .unwrap_or_default();
        let font_color = inline_html_attribute(inner, "color");
        let (color, background) =
            html::html_parser::inline_tag_colors(name, &styles, font_color.as_deref());
        InlineSpanStyle {
            color,
            background,
            paragraph_scale_percent: styles
                .get("font-size")
                .and_then(|value| html::html_parser::parse_css_font_scale(value)),
            text_scale_percent: None,
        }
    } else {
        InlineSpanStyle::default()
    };

    Some(InlineHtmlTag {
        name,
        destination,
        span_style,
        closing,
    })
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

fn inline_html_style_events(
    name: &str,
    span_style: InlineSpanStyle,
) -> Option<(MarkdownEvent, MarkdownEvent)> {
    Some(match name {
        "span" | "mark" | "font" => {
            if span_style == InlineSpanStyle::default() {
                return None;
            }
            (
                MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Span(span_style)),
                MarkdownEvent::InlineHtmlStyleEnd(InlineHtmlStyle::Span(span_style)),
            )
        }
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
        span_style: InlineSpanStyle,
        index: usize,
        depth: usize,
        in_heading: bool,
    }

    let mut open: Vec<OpenInlineTag> = Vec::new();
    let mut pairs = Vec::new();
    let mut depth = 0usize;
    let mut link_depth = 0usize;
    let mut heading_depth = 0usize;

    for index in 0..events.len() {
        let Some((range, event)) = events.get(index) else {
            break;
        };

        let is_dropped_tag = match event {
            MarkdownEvent::SubstitutedText(substituted) => substituted.is_empty(),
            MarkdownEvent::InlineHtml => true,
            MarkdownEvent::Start(tag) => {
                depth += 1;
                match tag {
                    MarkdownTag::Link { .. } => link_depth += 1,
                    MarkdownTag::Heading { .. } => heading_depth += 1,
                    _ => {}
                }
                false
            }
            MarkdownEvent::End(tag) => {
                depth = depth.saturating_sub(1);
                match tag {
                    MarkdownTagEnd::Link => link_depth = link_depth.saturating_sub(1),
                    MarkdownTagEnd::Heading(_) => heading_depth = heading_depth.saturating_sub(1),
                    _ => {}
                }
                open.retain(|open| open.depth <= depth);
                false
            }
            _ => false,
        };

        if !is_dropped_tag {
            continue;
        }

        let Some(InlineHtmlTag {
            name,
            destination,
            span_style,
            closing,
        }) = text
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
            // An unclosed `<span>`, `<mark>` or `<font>` left inside the element
            // being closed is passed over, the way a browser closes whatever is
            // still open inside it; since those three became recognized, a
            // stray `<span>` would otherwise cost `<b>x <span>y</b>` its bold.
            let position = open
                .iter()
                .rposition(|open| open.name == name && open.depth == depth);
            if let Some(position) = position
                && open.get(position + 1..).is_some_and(|inside| {
                    inside.iter().all(|inside| {
                        inside.depth == depth && matches!(inside.name, "span" | "mark" | "font")
                    })
                })
            {
                open.truncate(position + 1);
                if let Some(open) = open.pop() {
                    pairs.push((open, index));
                }
            }
        } else {
            open.push(OpenInlineTag {
                name,
                destination,
                span_style,
                index,
                depth,
                in_heading: heading_depth > 0,
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
            name => {
                let mut span_style = open.span_style;
                if span_style.paragraph_scale_percent.is_some()
                    && !wraps_whole_block_content(events, open_index, close_index)
                {
                    // A heading is drawn at the size of its level, which a
                    // browser would also give up for the span; keeping the
                    // heading's own size keeps its levels telling apart.
                    if !open.in_heading {
                        span_style.text_scale_percent = span_style.paragraph_scale_percent;
                    }
                    span_style.paragraph_scale_percent = None;
                }
                inline_html_style_events(name, span_style)
            }
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

/// Whether the events at `open_index` and `close_index` enclose everything a
/// paragraph or a list item holds before its first nested block: only then can
/// a `font-size` be drawn, by sizing the whole run of lines, since GPUI lays a
/// line out at a single font size.
fn wraps_whole_block_content(
    events: &[(Range<usize>, MarkdownEvent)],
    open_index: usize,
    close_index: usize,
) -> bool {
    // A task item's checkbox marker comes first, and is drawn as the bullet.
    let mut before = open_index.checked_sub(1);
    while let Some(index) = before
        && matches!(
            events.get(index),
            Some((_, MarkdownEvent::TaskListMarker(_)))
        )
    {
        before = index.checked_sub(1);
    }
    let opens_block = before
        .and_then(|index| events.get(index))
        .is_some_and(|(_, event)| {
            matches!(
                event,
                MarkdownEvent::Start(MarkdownTag::Paragraph | MarkdownTag::Item)
            )
        });

    let closes_block = events
        .get(close_index + 1)
        .is_some_and(|(_, event)| match event {
            MarkdownEvent::End(MarkdownTagEnd::Paragraph | MarkdownTagEnd::Item) => true,
            // A tight list item's text runs straight into its nested blocks.
            MarkdownEvent::Start(tag) => !matches!(
                tag,
                MarkdownTag::Emphasis
                    | MarkdownTag::Strong
                    | MarkdownTag::Strikethrough
                    | MarkdownTag::Superscript
                    | MarkdownTag::Subscript
                    | MarkdownTag::Link { .. }
                    | MarkdownTag::Image { .. }
            ),
            MarkdownEvent::Rule => true,
            _ => false,
        });

    opens_block && closes_block
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
    /// `<span>`, `<mark>` and `<font>` that ask for colors or a paragraph size.
    Span(InlineSpanStyle),
}

/// What a `<span>`, `<mark>` or `<font>` asks the preview to draw. Colors are
/// `0xRRGGBB`, a translucent one with its transparency in the top byte; a
/// `<mark>` carries the yellow background and black text a browser gives it
/// unless its `style` says otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineSpanStyle {
    pub color: Option<u32>,
    pub background: Option<u32>,
    /// The `font-size`, as a percentage of the surrounding text, of a span that
    /// wraps a paragraph's whole content, which sizes the paragraph's lines.
    pub paragraph_scale_percent: Option<u16>,
    /// The `font-size`, as a percentage of the surrounding text, of a span over
    /// part of a paragraph, whose text is drawn at that size within its lines.
    /// A span in a heading asks for neither size.
    pub text_scale_percent: Option<u16>,
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
    use gpui::TextAlign;

    /// Tables whose cells hold markdown written between the table's blocks.
    mod table_spans {
        use super::*;
        use html::html_parser::{
            HtmlColumnWidth, HtmlContainerName, HtmlContainerTag, HtmlParagraphChunk,
            HtmlSpanningCell, HtmlSpanningTable, HtmlTablePart, HtmlVerticalAlign,
            ParsedHtmlElement, html_details_key, html_paragraph_text,
        };

        fn table_parts(source: &str, block_source: &str) -> Vec<HtmlTablePart> {
            let block_start = source
                .find(block_source)
                .expect("the block is in the source");
            parse_markdown_with_options(source, true, false, false)
                .html_blocks
                .get(&block_start)
                .map(|block| block.table_parts.clone())
                .expect("the block should be an HTML block")
        }

        fn has_table_parts(source: &str) -> bool {
            parse_markdown_with_options(source, true, false, false)
                .html_blocks
                .values()
                .any(|block| !block.table_parts.is_empty())
        }

        fn cell(row: usize, column: usize) -> HtmlSpanningCell {
            HtmlSpanningCell {
                row,
                column,
                col_span: 1,
                row_span: 1,
                is_header: false,
                text_align: None,
                vertical_align: HtmlVerticalAlign::Middle,
            }
        }

        fn table(source_start: usize, column_count: usize) -> HtmlSpanningTable {
            HtmlSpanningTable {
                source_start,
                column_count,
                text_align: None,
                column_widths: vec![None; column_count],
                has_caption: false,
                caption_above: None,
            }
        }

        fn close_table() -> HtmlTablePart {
            HtmlTablePart::CloseTable {
                has_caption: false,
                caption_below: None,
            }
        }

        /// The README layout the change is for: a blank line after `<td>` ends
        /// the HTML block, so each cell's body arrives as markdown between the
        /// table's blocks.
        #[test]
        fn test_table_cells_hold_the_markdown_between_their_blocks() {
            let source = "<table>\n<tr>\n<td>\n\n**左の列** の本文\n\n- 項目\n\n</td>\n<td>\n\n![](右.png)\n\n</td>\n</tr>\n</table>\n\n後ろ\n";

            assert_eq!(
                table_parts(source, "<table>"),
                vec![
                    HtmlTablePart::OpenTable(table(0, 2)),
                    HtmlTablePart::OpenCell(cell(0, 0)),
                ]
            );
            assert_eq!(
                table_parts(source, "</td>\n<td>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(cell(0, 1))
                ]
            );
            assert_eq!(
                table_parts(source, "</td>\n</tr>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 0,
                        occupied_columns: vec![true, true],
                    },
                    close_table(),
                ]
            );
        }

        #[test]
        fn test_table_rows_headers_and_cell_attributes_span_blocks() {
            let source = "<table>\n<thead>\n<tr>\n<th colspan=\"2\">\n\n見出し\n\n</th>\n</tr>\n</thead>\n<tbody>\n<tr>\n<td valign=\"top\" width=\"120\">\n\n左\n\n</td>\n<td align=\"right\">\n\n右\n\n</td>\n</tr>\n<tr>\n<td>\n\n下\n\n</td>\n</tr>\n</tbody>\n</table>\n";

            assert_eq!(
                table_parts(source, "<table>"),
                vec![
                    HtmlTablePart::OpenTable(HtmlSpanningTable {
                        column_widths: vec![Some(HtmlColumnWidth::Pixels(120.0)), None],
                        ..table(0, 2)
                    }),
                    HtmlTablePart::OpenCell(HtmlSpanningCell {
                        col_span: 2,
                        is_header: true,
                        text_align: Some(TextAlign::Center),
                        ..cell(0, 0)
                    }),
                ]
            );
            assert_eq!(
                table_parts(source, "</th>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 0,
                        occupied_columns: vec![true, true],
                    },
                    HtmlTablePart::OpenCell(HtmlSpanningCell {
                        vertical_align: HtmlVerticalAlign::Top,
                        ..cell(1, 0)
                    }),
                ]
            );
            assert_eq!(
                table_parts(source, "</td>\n<td align"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(HtmlSpanningCell {
                        text_align: Some(TextAlign::Right),
                        ..cell(1, 1)
                    }),
                ]
            );
            assert_eq!(
                table_parts(source, "</td>\n</tr>\n<tr>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 1,
                        occupied_columns: vec![true, true],
                    },
                    HtmlTablePart::OpenCell(cell(2, 0)),
                ]
            );
            assert_eq!(
                table_parts(source, "</td>\n</tr>\n</tbody>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 2,
                        occupied_columns: vec![true, false],
                    },
                    close_table(),
                ]
            );
        }

        /// `<td>` closes the cell before it and `</tr>` its last cell, the way
        /// a browser reads them, so leaving out `</td>` still pairs.
        #[test]
        fn test_cells_close_by_implication() {
            let source = "<table>\n<tr>\n<td>\n\n左\n\n<td>\n\n右\n\n</tr>\n</table>\n";
            assert_eq!(
                table_parts(source, "<td>\n\n右"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(cell(0, 1))
                ]
            );
            assert_eq!(
                table_parts(source, "</tr>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 0,
                        occupied_columns: vec![true, true],
                    },
                    close_table(),
                ]
            );
        }

        /// A cell written whole in the opening block keeps its HTML, and sits
        /// in the same table as the markdown cell after it.
        #[test]
        fn test_html_cells_and_markdown_cells_share_a_table() {
            let source = "<table><tr><td align=\"center\">本文 <b>太字</b></td><td>\n\n**markdown**\n\n</td></tr></table>\n";
            let block = parse_markdown_with_options(source, true, false, false)
                .html_blocks
                .get(&0)
                .cloned()
                .expect("the table block is an HTML block");

            assert_eq!(
                block.table_parts,
                vec![
                    HtmlTablePart::OpenTable(table(0, 2)),
                    HtmlTablePart::OpenCell(HtmlSpanningCell {
                        text_align: Some(TextAlign::Center),
                        ..cell(0, 0)
                    }),
                    HtmlTablePart::Elements(0..1),
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(cell(0, 1)),
                ]
            );
            match block.children.as_slice() {
                [ParsedHtmlElement::Paragraph(paragraph)] => {
                    assert_eq!(html_paragraph_text(&paragraph.contents), "本文 太字");
                    assert_eq!(paragraph.text_align, Some(TextAlign::Center));
                }
                other => panic!("expected the cell's paragraph, got {other:?}"),
            }

            // A cell that does not say takes the alignment of a container an
            // earlier block opened around the table.
            let source = "<div align=\"right\">\n\n<table><tr><td>本文</td><td>\n\n**markdown**\n\n</td></tr></table>\n\n</div>\n";
            let table_start = source.find("<table>").unwrap_or_default();
            let parsed = parse_markdown_with_options(source, true, false, false);
            let children = parsed
                .html_blocks
                .get(&table_start)
                .map(|block| block.children.clone())
                .unwrap_or_default();
            match children.as_slice() {
                [ParsedHtmlElement::Paragraph(paragraph)] => {
                    assert_eq!(paragraph.text_align, Some(TextAlign::Right));
                }
                other => panic!("expected the cell's paragraph, got {other:?}"),
            }
        }

        /// The HTML around the table in its own blocks is still drawn, before
        /// and after the table.
        #[test]
        fn test_html_around_the_table_stays_in_its_place() {
            let source = "<p>前</p>\n<table><tr><td>\n\n中\n\n</td></tr></table>\n<p>後</p>\n";
            let parsed = parse_markdown_with_options(source, true, false, false);
            let texts = |block_start: usize| {
                let block = parsed.html_blocks.get(&block_start).expect("an HTML block");
                block
                    .table_parts
                    .iter()
                    .filter_map(|part| match part {
                        HtmlTablePart::Elements(range) => block.children.get(range.clone()),
                        _ => None,
                    })
                    .flatten()
                    .map(|element| match element {
                        ParsedHtmlElement::Paragraph(paragraph) => {
                            html_paragraph_text(&paragraph.contents)
                        }
                        other => panic!("unexpected element {other:?}"),
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(texts(0), vec!["前".to_string()]);
            let closing = source.find("</td>").expect("the closing block");
            assert_eq!(texts(closing), vec!["後".to_string()]);
            assert!(matches!(
                parsed
                    .html_blocks
                    .get(&0)
                    .map(|block| block.table_parts.as_slice()),
                Some([HtmlTablePart::Elements(_), HtmlTablePart::OpenTable(_), ..])
            ));
        }

        #[test]
        fn test_row_spans_push_the_cells_below_them_to_the_right() {
            let source = "<table>\n<tr>\n<td rowspan=\"2\">\n\nA\n\n</td>\n<td>\n\nB\n\n</td>\n</tr>\n<tr>\n<td rowspan=\"9\">\n\nC\n\n</td>\n</tr>\n<tr>\n<td>\n\nD\n\n</td>\n</tr>\n</table>\n";

            assert_eq!(
                table_parts(source, "<table>"),
                vec![
                    HtmlTablePart::OpenTable(table(0, 2)),
                    HtmlTablePart::OpenCell(HtmlSpanningCell {
                        row_span: 2,
                        ..cell(0, 0)
                    }),
                ]
            );
            assert_eq!(
                table_parts(source, "</td>\n</tr>\n<tr>\n<td rowspan"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 0,
                        occupied_columns: vec![true, true],
                    },
                    HtmlTablePart::OpenCell(HtmlSpanningCell {
                        // Past the last row the span stops at the table's end.
                        row_span: 2,
                        ..cell(1, 1)
                    }),
                ],
                "the cell under `B` sits beside `A`, which still covers column 0"
            );
            assert_eq!(
                table_parts(source, "</td>\n</tr>\n<tr>\n<td>\n\nD"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 1,
                        occupied_columns: vec![true, true],
                    },
                    HtmlTablePart::OpenCell(cell(2, 0)),
                ],
                "`A` no longer covers the third row"
            );
        }

        /// Every cell's place and every row's taken columns, across all of the
        /// table's blocks, in document order.
        fn table_layout(source: &str) -> Vec<(&'static str, usize, usize, usize, usize)> {
            let parsed = parse_markdown_with_options(source, true, false, false);
            let mut blocks = parsed.html_blocks.iter().collect::<Vec<_>>();
            blocks.sort_by_key(|(start, _)| **start);
            blocks
                .into_iter()
                .flat_map(|(_, block)| block.table_parts.iter())
                .filter_map(|part| match part {
                    HtmlTablePart::OpenCell(cell) => {
                        Some(("cell", cell.row, cell.column, cell.row_span, cell.col_span))
                    }
                    HtmlTablePart::EndRow {
                        row,
                        occupied_columns,
                    } => Some((
                        "row",
                        *row,
                        occupied_columns
                            .iter()
                            .enumerate()
                            .filter(|(_, taken)| **taken)
                            .map(|(column, _)| 1 << column)
                            .sum(),
                        occupied_columns.len(),
                        0,
                    )),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn test_row_spans_with_col_spans_and_rows_that_end_early() {
            let cell = |contents: &str, attributes: &str| {
                format!("<td{attributes}>\n\n{contents}\n\n</td>\n")
            };
            let source = [
                "<table>\n<tr>\n".to_string(),
                cell("A", " rowspan=\"2\" colspan=\"2\""),
                cell("B", ""),
                "</tr>\n<tr>\n".to_string(),
                cell("C", ""),
                "</tr>\n<tr>\n".to_string(),
                cell("D", ""),
                cell("E", " rowspan=\"2\""),
                cell("F", ""),
                "</tr>\n<tr>\n".to_string(),
                // Ends before the column `E` still covers, and the one after it.
                cell("G", ""),
                "</tr>\n<tr>\n".to_string(),
                cell("H", ""),
                cell("I", " rowspan=\"7\" colspan=\"2\""),
                "</tr>\n</table>\n".to_string(),
            ]
            .concat();
            // Columns taken in each ended row are a bit set: 0b111 is all three.
            assert_eq!(
                table_layout(&source),
                vec![
                    ("cell", 0, 0, 2, 2),
                    ("cell", 0, 2, 1, 1),
                    ("row", 0, 0b111, 3, 0),
                    ("cell", 1, 2, 1, 1),
                    ("row", 1, 0b111, 3, 0),
                    ("cell", 2, 0, 1, 1),
                    ("cell", 2, 1, 2, 1),
                    ("cell", 2, 2, 1, 1),
                    ("row", 2, 0b111, 3, 0),
                    ("cell", 3, 0, 1, 1),
                    ("row", 3, 0b011, 3, 0),
                    ("cell", 4, 0, 1, 1),
                    // Past the last row the span stops at the table's end.
                    ("cell", 4, 1, 1, 2),
                    ("row", 4, 0b111, 3, 0),
                ]
            );
        }

        #[test]
        fn test_percentage_widths_reach_the_columns() {
            let source = "<table>\n<tr>\n<td width=\"30%\">\n\nA\n\n</td>\n<td style=\"width: 200px\">\n\nB\n\n</td>\n<td>\n\nC\n\n</td>\n</tr>\n<tr>\n<td width=\"120\">\n\nD\n\n</td>\n</tr>\n</table>\n";
            assert_eq!(
                table_parts(source, "<table>").first(),
                Some(&HtmlTablePart::OpenTable(HtmlSpanningTable {
                    column_widths: vec![
                        Some(HtmlColumnWidth::Fraction(0.3)),
                        Some(HtmlColumnWidth::Pixels(200.0)),
                        None,
                    ],
                    ..table(0, 3)
                }))
            );
        }

        fn caption_text(source: &str, block_source: &str, range: &Range<usize>) -> String {
            let block_start = source
                .find(block_source)
                .expect("the block is in the source");
            let parsed = parse_markdown_with_options(source, true, false, false);
            let block = parsed
                .html_blocks
                .get(&block_start)
                .expect("the block is an HTML block");
            block
                .children
                .get(range.clone())
                .unwrap_or_default()
                .iter()
                .map(|element| match element {
                    ParsedHtmlElement::Paragraph(paragraph) => {
                        assert_eq!(paragraph.text_align, Some(TextAlign::Center));
                        html_paragraph_text(&paragraph.contents)
                    }
                    other => panic!("expected the caption's paragraph, got {other:?}"),
                })
                .collect()
        }

        #[test]
        fn test_a_caption_keeps_the_table_and_is_drawn_above_or_below_it() {
            let source = "<div align=\"right\">\n\n<table>\n<caption><b>表題</b></caption>\n<tr>\n<td>\n\nA\n\n</td>\n</tr>\n</table>\n\n</div>\n";
            let parts = table_parts(source, "<table>");
            let Some(HtmlTablePart::OpenTable(HtmlSpanningTable {
                has_caption: true,
                caption_above: Some(caption),
                ..
            })) = parts.first()
            else {
                panic!("expected a table with its caption above, got {parts:?}");
            };
            assert_eq!(
                caption_text(source, "<table>", caption),
                "表題",
                "the caption is centered even where the div aligns right"
            );
            assert!(
                !parts
                    .iter()
                    .any(|part| matches!(part, HtmlTablePart::Elements(_))),
                "the caption is not drawn a second time as the table's HTML: {parts:?}"
            );
            assert_eq!(
                table_parts(source, "</td>").last(),
                Some(&HtmlTablePart::CloseTable {
                    has_caption: true,
                    caption_below: None,
                })
            );

            let source = "<table>\n<caption align=\"bottom\">下の表題</caption>\n<tr>\n<td>\n\nA\n\n</td>\n</tr>\n</table>\n";
            let parts = table_parts(source, "<table>");
            assert!(
                matches!(
                    parts.first(),
                    Some(HtmlTablePart::OpenTable(HtmlSpanningTable {
                        has_caption: true,
                        caption_above: None,
                        ..
                    }))
                ),
                "{parts:?}"
            );
            let parts = table_parts(source, "</td>");
            let Some(HtmlTablePart::CloseTable {
                has_caption: true,
                caption_below: Some(caption),
            }) = parts.last()
            else {
                panic!("expected the caption below the table, got {parts:?}");
            };
            assert_eq!(caption_text(source, "</td>", caption), "下の表題");

            // A row closes a caption left open, as in a browser.
            let source = "<table><caption>開いたまま<tr><td>\n\nA\n\n</td></tr></table>\n";
            let parts = table_parts(source, "<table>");
            let Some(HtmlTablePart::OpenTable(HtmlSpanningTable {
                caption_above: Some(caption),
                ..
            })) = parts.first()
            else {
                panic!("expected a table with its caption above, got {parts:?}");
            };
            assert_eq!(caption_text(source, "<table>", caption), "開いたまま");
        }

        /// A cell bounds what its tags close, so a stray `</div>` in it leaves
        /// the `<div>` opened before the table open around the cells after it.
        #[test]
        fn test_a_div_close_in_a_cell_leaves_a_div_around_the_table_open() {
            let source = "<div align=\"center\">\n\n<table><tr><td>\n\nA\n\n</div>\n\nB\n\n</td><td>\n\nC\n\n</td></tr></table>\n\nD\n\n</div>\n\n後ろ\n";
            assert_eq!(
                aligned_texts(source),
                vec![
                    ("A", Some(TextAlign::Center)),
                    ("B", Some(TextAlign::Center)),
                    ("C", Some(TextAlign::Center)),
                    ("D", Some(TextAlign::Center)),
                    ("後ろ", None),
                ]
            );
        }

        /// What cannot be paired is drawn as it was before rather than losing
        /// what does not fit in a cell.
        #[test]
        fn test_table_that_does_not_pair_is_drawn_as_before() {
            for source in [
                // Never closed.
                "<table>\n<tr>\n<td>\n\n本文\n",
                "<table>\n<tr>\n<td>\n\n本文\n\n</td>\n</tr>\n",
                // Markdown between two cells has no cell to go in.
                "<table>\n<tr>\n<td>\n\nA\n\n</td>\n\n外\n\n<td>\n\nB\n\n</td>\n</tr>\n</table>\n",
                // Text between the table and its first row.
                "<table>文字<tr><td>\n\nA\n\n</td></tr></table>\n",
                // Opened in a list item, closed outside it.
                "- <table><tr><td>\n\n  本文\n\n</td></tr></table>\n",
                // The document ends in the middle of a nested table.
                "<table><tr><td>\n\n<table><tr><td>\n\n中\n",
                // `</td>` with no table.
                "本文\n\n</td></tr></table>\n",
                // Closed from inside a list in the cell.
                "<table><tr><td>\n\n- </td></tr></table>\n",
                // Whole in one block, which the HTML parser draws by itself.
                "<table><tr><td>本文</td></tr></table>\n\n後ろ\n",
                // A caption whose content goes on into the next blocks.
                "<table>\n<caption>\n\n表題\n\n</caption>\n<tr>\n<td>\n\nA\n\n</td>\n</tr>\n</table>\n",
                "<table>\n<caption>表題\n\n</caption>\n<tr>\n<td>\n\nA\n\n</td>\n</tr>\n</table>\n",
                // A second caption has no place beside the table.
                "<table>\n<caption>一</caption><caption>二</caption>\n<tr>\n<td>\n\nA\n\n</td>\n</tr>\n</table>\n",
            ] {
                assert!(!has_table_parts(source), "{source:?}");
                let parsed = parse_markdown_with_options(source, true, false, false);
                for block in parsed.html_blocks.values() {
                    assert!(
                        !block.container_tags.iter().any(|tag| matches!(
                            tag,
                            HtmlContainerTag::Open {
                                name: HtmlContainerName::TableCell,
                                ..
                            } | HtmlContainerTag::Close(HtmlContainerName::TableCell)
                        )),
                        "{source:?}"
                    );
                }
            }
        }

        /// A `</div>` inside a cell cannot close a `<div>` around the table,
        /// the way a browser ignores it, so the table still pairs.
        #[test]
        fn test_a_stray_div_close_in_a_cell_keeps_the_table() {
            let source = "<div align=\"center\"><table><tr><td>\n\n中\n\n</div>\n\n</td><td>\n\n右\n\n</td></tr></table></div>\n";
            assert_eq!(
                table_parts(source, "</td><td>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(cell(0, 1))
                ]
            );
        }

        #[test]
        fn test_table_with_crlf_line_endings_pairs() {
            let source = "<table>\r\n<tr>\r\n<td>\r\n\r\n**左** の本文\r\n\r\n</td>\r\n<td>\r\n\r\n右\r\n\r\n</td>\r\n</tr>\r\n</table>\r\n\r\n後ろ\r\n";
            assert_eq!(
                table_parts(source, "</td>\r\n<td>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(cell(0, 1))
                ]
            );
        }

        #[test]
        fn test_comments_and_row_groups_between_rows_still_pair() {
            let source = "<table>\n<!-- 表 -->\n<tbody>\n<tr>\n<td>\n\nA\n\n</td>\n</tr>\n</tbody>\n</table>\n";
            assert!(has_table_parts(source));
        }

        #[test]
        fn test_markdown_in_an_aligned_cell_is_aligned() {
            let source = "<table>\n<tr>\n<th>\n\n見出し\n\n</th>\n<td align=\"center\">\n\n中央\n\n</td>\n<td align=\"right\">\n\n右\n\n<div align=\"center\">\n\n中の中央\n\n</div>\n\n右2\n\n</td>\n<td>\n\n左\n\n</td>\n</tr>\n</table>\n\n後ろ\n";
            assert_eq!(
                aligned_texts(source),
                vec![
                    ("見出し", Some(TextAlign::Center)),
                    ("中央", Some(TextAlign::Center)),
                    ("右", Some(TextAlign::Right)),
                    ("中の中央", Some(TextAlign::Center)),
                    ("右2", Some(TextAlign::Right)),
                    ("左", None),
                    ("後ろ", None),
                ]
            );
        }

        /// A `<div>` written in the same block as the `<td>` sits inside the
        /// cell or around the table by where it is written, and `</td>` closes
        /// one the cell leaves open.
        #[test]
        fn test_a_div_in_the_cells_block_aligns_by_where_it_is_written() {
            let source = "<table><tr><td align=\"right\"><div align=\"center\">\n\n中\n\n</div>\n\n右\n\n</td><td>\n\n左\n\n</td></tr></table>\n";
            assert_eq!(
                aligned_texts(source),
                vec![
                    ("中", Some(TextAlign::Center)),
                    ("右", Some(TextAlign::Right)),
                    ("左", None),
                ]
            );

            let source = "<div align=\"center\"><table><tr><td align=\"left\">\n\n左\n\n</td><td>\n\n中\n\n</td></tr></table></div>\n\n後ろ\n";
            assert_eq!(
                aligned_texts(source),
                vec![
                    ("左", Some(TextAlign::Left)),
                    ("中", Some(TextAlign::Center)),
                    ("後ろ", None),
                ]
            );
            assert_eq!(
                table_parts(source, "<div").first(),
                Some(&HtmlTablePart::OpenTable(HtmlSpanningTable {
                    text_align: Some(TextAlign::Center),
                    ..table(source.find("<table").unwrap_or_default(), 2)
                }))
            );

            let source = "<table><tr><td><div align=\"center\">\n\n中\n\n</td><td>\n\n左\n\n</td></tr></table>\n";
            assert_eq!(
                aligned_texts(source),
                vec![("中", Some(TextAlign::Center)), ("左", None)]
            );
        }

        #[test]
        fn test_an_aligned_div_around_the_table_reaches_its_cells() {
            let source = "<div align=\"center\">\n\n<table>\n<tr>\n<td>\n\n中\n\n</td>\n<td align=\"left\">\n\n左\n\n</td>\n</tr>\n</table>\n\n</div>\n\n後ろ\n";
            assert_eq!(
                aligned_texts(source),
                vec![
                    ("中", Some(TextAlign::Center)),
                    ("左", Some(TextAlign::Left)),
                    ("後ろ", None),
                ]
            );
            // The renderer centers the table itself from the aligned span.
            assert_eq!(
                table_parts(source, "<table>").first(),
                Some(&HtmlTablePart::OpenTable(table(
                    source.find("<table>").unwrap_or_default(),
                    2
                )))
            );
        }

        #[test]
        fn test_details_and_tables_nest_within_each_other() {
            let details_blocks = |source: &str| {
                parse_markdown_with_options(source, true, false, false)
                    .html_blocks
                    .values()
                    .filter(|block| block.details_span.is_some())
                    .count()
            };

            // A disclosure inside a cell.
            let source = "<table><tr><td>\n\n<details><summary>もっと</summary>\n\n隠れる\n\n</details>\n\n</td><td>\n\n右\n\n</td></tr></table>\n";
            assert!(has_table_parts(source));
            assert_eq!(details_blocks(source), 1);

            // A disclosure opened by the table's own block, as the last thing
            // in it, is still the one its body belongs to.
            let source = "<table><tr><td><details><summary>もっと</summary>\n\n隠れる\n\n</details>\n\n</td></tr></table>\n\n<details><summary>もっと</summary>別</details>\n";
            let parsed = parse_markdown_with_options(source, true, false, false);
            let table_block = parsed.html_blocks.get(&0).expect("the table block");
            assert!(table_block.details_span.is_some());
            let Some(HtmlTablePart::Elements(range)) = table_block.table_parts.last() else {
                panic!("the disclosure should end the block: {table_block:?}");
            };
            let Some(ParsedHtmlElement::Details(details)) = table_block
                .children
                .get(range.clone())
                .and_then(|elements| elements.last())
            else {
                panic!("the disclosure should end the block: {table_block:?}");
            };
            assert_eq!(details.key, html_details_key("もっと", 0));
            let later =
                parsed
                    .html_blocks
                    .values()
                    .last()
                    .and_then(|block| match block.children.first() {
                        Some(ParsedHtmlElement::Details(details)) => Some(details.key),
                        _ => None,
                    });
            assert_eq!(later, Some(html_details_key("もっと", 1)));

            // A table inside a disclosure.
            let source = "<details><summary>表</summary>\n\n<table><tr><td>\n\n中\n\n</td></tr></table>\n\n</details>\n";
            assert!(has_table_parts(source));

            // A disclosure that closes outside the cell it opened in would hide
            // some of the table's tags and not others.
            let source = "<details><summary>表</summary>\n\n<table><tr><td>\n\n中\n\n</details>\n\n</td></tr></table>\n";
            assert!(!has_table_parts(source));
            let source = "<table><tr><td><details><summary>表</summary>\n\n中\n\n</td></tr></table>\n\n</details>\n";
            assert!(!has_table_parts(source));
            // Opened in one cell with its body in the next.
            let source = "<table><tr><td><details><summary>表</summary></td><td>\n\n中\n\n</details>\n\n</td></tr></table>\n";
            assert!(!has_table_parts(source));
            let source = "<table><tr><td><details><summary>表</summary></td><td><details><summary>別</summary>閉じた</details>\n\n中\n\n</details>\n\n</td></tr></table>\n";
            assert!(!has_table_parts(source));
        }

        #[test]
        fn test_a_table_spanning_blocks_inside_a_cell() {
            let source = "<table><tr><td>\n\n<table><tr><td>\n\n内\n\n</td></tr></table>\n\n</td><td>\n\n外\n\n</td></tr></table>\n";
            let inner_start = source.find("<table><tr><td>\n\n内").unwrap_or_default();
            assert_eq!(
                table_parts(source, "<table><tr><td>\n\n内"),
                vec![
                    HtmlTablePart::OpenTable(table(inner_start, 1)),
                    HtmlTablePart::OpenCell(cell(0, 0)),
                ]
            );
            assert_eq!(
                table_parts(source, "</td></tr></table>\n\n</td>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::EndRow {
                        row: 0,
                        occupied_columns: vec![true],
                    },
                    close_table(),
                ]
            );
            assert_eq!(
                table_parts(source, "</td><td>"),
                vec![
                    HtmlTablePart::CloseCell,
                    HtmlTablePart::OpenCell(cell(0, 1))
                ]
            );
        }

        /// An image written as its own HTML block inside an aligned cell is
        /// aligned like one written inside the cell's own block.
        #[test]
        fn test_an_html_image_block_in_an_aligned_cell_is_aligned() {
            let source =
                "<table><tr><td align=\"center\">\n\n<img src=\"a.png\">\n\n</td></tr></table>\n";
            let image_paragraph_align =
                html_block_children(source, "<img")
                    .iter()
                    .find_map(|element| match element {
                        ParsedHtmlElement::Paragraph(paragraph)
                            if matches!(
                                paragraph.contents.as_slice(),
                                [HtmlParagraphChunk::Image(_)]
                            ) =>
                        {
                            paragraph.text_align
                        }
                        _ => None,
                    });
            assert_eq!(image_paragraph_align, Some(TextAlign::Center));
        }
    }

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

    /// Each markdown text of `source` with the alignment the aligned spans give
    /// it, the innermost span winning the way the renderer picks it.
    fn aligned_texts(source: &str) -> Vec<(&str, Option<gpui::TextAlign>)> {
        let parsed = parse_markdown_with_options(source, true, false, false);
        let spans = parsed
            .html_blocks
            .values()
            .flat_map(|block| block.aligned_spans.iter())
            .collect::<Vec<_>>();

        parsed
            .events
            .iter()
            .enumerate()
            .filter(|(_, (_, event))| matches!(event, MarkdownEvent::Text))
            .map(|(index, (range, _))| {
                let text_align = spans
                    .iter()
                    .filter(|span| span.events.contains(&index))
                    .max_by_key(|span| span.nesting)
                    .map(|span| span.text_align);
                (&source[range.clone()], text_align)
            })
            .collect()
    }

    fn html_block_children(
        source: &str,
        block_source: &str,
    ) -> Vec<html::html_parser::ParsedHtmlElement> {
        let block_start = source
            .find(block_source)
            .expect("the block is in the source");
        parse_markdown_with_options(source, true, false, false)
            .html_blocks
            .get(&block_start)
            .map(|block| block.children.clone())
            .expect("the block should be an HTML block")
    }

    fn aligned_image_paragraph(
        element: &html::html_parser::ParsedHtmlElement,
    ) -> Option<TextAlign> {
        use html::html_parser::{HtmlParagraphChunk, ParsedHtmlElement};

        match element {
            ParsedHtmlElement::Paragraph(paragraph)
                if matches!(
                    paragraph.contents.as_slice(),
                    [HtmlParagraphChunk::Image(_)]
                ) =>
            {
                paragraph.text_align
            }
            _ => None,
        }
    }

    /// The README shape the fix is for: blank lines inside `<div align>` end
    /// the HTML block, so the heading, the image, the text, the list and the
    /// table in between used to be left-aligned.
    #[test]
    fn test_aligned_div_spans_the_markdown_between_its_blocks() {
        let source = "<div align=\"center\">\n\n# プロジェクト名\n\n<img src=\"logo.png\" width=\"200\">\n\n説明文\n\n- 項目\n\n| 列 |\n|---|\n| 値 |\n\n</div>\n\n後ろ\n";

        assert_eq!(
            aligned_texts(source),
            vec![
                ("プロジェクト名", Some(TextAlign::Center)),
                ("説明文", Some(TextAlign::Center)),
                ("項目", Some(TextAlign::Center)),
                ("列", Some(TextAlign::Center)),
                ("値", Some(TextAlign::Center)),
                ("後ろ", None),
            ]
        );

        let image_block = html_block_children(source, "<img");
        assert_eq!(
            image_block
                .iter()
                .map(aligned_image_paragraph)
                .collect::<Vec<_>>(),
            vec![Some(TextAlign::Center)],
            "the image written as its own HTML block is aligned like one inside the div: {image_block:?}"
        );
    }

    #[test]
    fn test_aligned_div_without_its_other_half_aligns_nothing() {
        for source in [
            "<div align=\"center\">\n\n本文\n\n後ろ\n",
            "本文\n\n</div>\n\n後ろ\n",
            "<div align=\"center\">\n\n本文\n\n</center>\n\n後ろ\n",
        ] {
            assert_eq!(
                aligned_texts(source),
                vec![("本文", None), ("後ろ", None)],
                "{source:?}"
            );
        }
    }

    #[test]
    fn test_div_tags_inside_code_blocks_are_not_containers() {
        let source = "<div align=\"center\">\n\n```\n</div>\n```\n\n中\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("</div>\n", Some(TextAlign::Center)),
                ("中", Some(TextAlign::Center)),
                ("後ろ", None),
            ]
        );

        let source = "```\n<div align=\"center\">\n```\n\n本文\n\n</div>\n";
        assert_eq!(
            aligned_texts(source),
            vec![("<div align=\"center\">\n", None), ("本文", None)]
        );
    }

    #[test]
    fn test_nested_aligned_divs_align_by_the_innermost() {
        let source = "<div align=\"center\">\n\n外1\n\n<div align=\"right\">\n\n内\n\n<img src=\"a.png\">\n\n</div>\n\n外2\n\n</div>\n";

        assert_eq!(
            aligned_texts(source),
            vec![
                ("外1", Some(TextAlign::Center)),
                ("内", Some(TextAlign::Right)),
                ("外2", Some(TextAlign::Center)),
            ]
        );
        assert_eq!(
            html_block_children(source, "<img")
                .iter()
                .map(aligned_image_paragraph)
                .collect::<Vec<_>>(),
            vec![Some(TextAlign::Right)]
        );

        // Both opened by one block: the inner one closes first, so it cannot win
        // by coming last.
        let source = "<div align=\"center\"><div align=\"right\">\n\n内\n\n<img src=\"a.png\">\n\n</div>\n\n外\n\n</div>\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("内", Some(TextAlign::Right)),
                ("外", Some(TextAlign::Center)),
            ]
        );
        assert_eq!(
            html_block_children(source, "<img")
                .iter()
                .map(aligned_image_paragraph)
                .collect::<Vec<_>>(),
            vec![Some(TextAlign::Right)]
        );
    }

    /// The HTML elements of the block starting with `block_source`, each as
    /// an image or text with its alignment.
    fn html_block_alignments(
        source: &str,
        block_source: &str,
    ) -> Vec<(&'static str, Option<TextAlign>)> {
        use html::html_parser::{HtmlParagraphChunk, ParsedHtmlElement};

        html_block_children(source, block_source)
            .iter()
            .map(|element| match element {
                ParsedHtmlElement::Image(_) => ("image", None),
                ParsedHtmlElement::Paragraph(paragraph) => {
                    let is_image = matches!(
                        paragraph.contents.as_slice(),
                        [HtmlParagraphChunk::Image(_)]
                    );
                    (
                        if is_image { "image" } else { "text" },
                        paragraph.text_align,
                    )
                }
                ParsedHtmlElement::Details(_) => ("details", None),
                other => panic!("unexpected element {other:?}"),
            })
            .collect()
    }

    /// The README shape where the last image sits right above `</div>`: the
    /// part of the closing block before the tag is inside the container.
    #[test]
    fn test_closing_block_aligns_what_comes_before_its_close() {
        let source =
            "<div align=\"center\">\n\n本文\n\n<img src=\"a.png\">\n</div>\n<p>後ろ</p>\n\n最後\n";
        assert_eq!(
            html_block_alignments(source, "<img"),
            vec![("image", Some(TextAlign::Center)), ("text", None)],
            "the part after the close stays as it was"
        );
        assert_eq!(
            aligned_texts(source),
            vec![("本文", Some(TextAlign::Center)), ("最後", None)]
        );

        // Closing the inner container first, then the outer one.
        let source = "<div align=\"center\">\n\n<div align=\"right\">\n\n中\n\n<img src=\"a.png\">\n</div>\n<img src=\"b.png\">\n</div>\n";
        assert_eq!(
            html_block_alignments(source, "<img"),
            vec![
                ("image", Some(TextAlign::Right)),
                ("image", Some(TextAlign::Center)),
            ]
        );

        // A plain `<div>` in between still takes the first `</div>`.
        let source = "<div align=\"right\">\n\n<div>\n\n中\n\n<img src=\"a.png\">\n</div>\n<img src=\"b.png\">\n</div>\n";
        assert_eq!(
            html_block_alignments(source, "<img"),
            vec![
                ("image", Some(TextAlign::Right)),
                ("image", Some(TextAlign::Right)),
            ]
        );

        // Closing only the inner container: what follows the close is still
        // inside the outer one.
        let source = "<div align=\"center\">\n\n<div align=\"right\">\n\n中\n\n<img src=\"a.png\">\n</div>\n<img src=\"b.png\">\n\n外\n\n</div>\n";
        assert_eq!(
            html_block_alignments(source, "<img"),
            vec![
                ("image", Some(TextAlign::Right)),
                ("image", Some(TextAlign::Center)),
            ]
        );

        // A stray close aligns nothing, and a block closing a plain `<div>`
        // is left as it was.
        for source in [
            "本文\n\n<img src=\"a.png\">\n</div>\n",
            "<div>\n\n本文\n\n<img src=\"a.png\">\n</div>\n",
        ] {
            assert_eq!(
                html_block_alignments(source, "<img"),
                vec![("image", None)],
                "{source:?}"
            );
        }

        // A closing block holding a disclosure is aligned too.
        let source = "<div align=\"center\">\n\n本文\n\n<details><summary>題</summary>中</details>\n<img src=\"a.png\">\n</div>\n";
        assert_eq!(
            html_block_alignments(source, "<details"),
            vec![("details", None), ("image", Some(TextAlign::Center))]
        );
    }

    /// A closing block is parsed again behind the containers it closes, which
    /// must neither renumber the disclosures in it nor keep aligning what
    /// follows a `</details>` that closes the aligned `<div>` inside it.
    #[test]
    fn test_closing_block_with_details_keeps_their_keys() {
        use html::html_parser::{ParsedHtmlElement, html_details_key};

        let source = "<details><summary>題</summary>一</details>\n\n<div align=\"center\">\n\n本文\n\n<details><summary>題</summary>二</details>\n</div>\n";
        let second = source
            .rfind("<details")
            .expect("the second disclosure is there");
        let parsed = parse_markdown_with_options(source, true, false, false);
        let block = parsed
            .html_blocks
            .get(&second)
            .expect("the closing block is an HTML block");
        let [ParsedHtmlElement::Details(details)] = block.children.as_slice() else {
            panic!("expected only the disclosure, got {:?}", block.children);
        };
        assert_eq!(
            details.key,
            html_details_key("題", 1),
            "the disclosure is still the second one with its summary"
        );
        let [ParsedHtmlElement::Paragraph(body)] = details.children.as_slice() else {
            panic!("expected the disclosure body, got {:?}", details.children);
        };
        assert_eq!(body.text_align, Some(TextAlign::Center));

        let source = "<details><summary>題</summary>\n\n<div align=\"center\">\n\n本文\n\n<img src=\"a.png\">\n</details>\n<p>後ろ</p>\n";
        assert_eq!(
            html_block_alignments(source, "<img"),
            vec![("image", Some(TextAlign::Center)), ("text", None)],
            "`</details>` closes the `<div>` opened inside it"
        );
        assert_eq!(
            aligned_texts(source),
            vec![("本文", Some(TextAlign::Center))]
        );
    }

    #[test]
    fn test_tags_inside_raw_text_and_pre_open_no_containers() {
        for source in [
            "<script>\nlet html = \"<div align='center'>\";\n</script>\n\n本文\n\n</div>\n\n後ろ\n",
            "<style>\n/* <div align=\"center\"> */\n</style>\n\n本文\n\n</div>\n\n後ろ\n",
            "<textarea>\n<div align=\"center\">\n</textarea>\n\n本文\n\n</div>\n\n後ろ\n",
            "<pre>\n<div align=\"center\">\n</pre>\n\n本文\n\n</div>\n\n後ろ\n",
        ] {
            assert_eq!(
                aligned_texts(source),
                vec![("本文", None), ("後ろ", None)],
                "{source:?}"
            );
        }

        // Nor do their closing tags close anything.
        let source =
            "<div align=\"center\">\n\n本文\n\n<pre>\n</div>\n</pre>\n\n中\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("本文", Some(TextAlign::Center)),
                ("中", Some(TextAlign::Center)),
                ("後ろ", None),
            ]
        );
    }

    /// A script left open reads the rest of its own block as text, but the
    /// next block is read as markup again.
    #[test]
    fn test_unclosed_script_only_hides_the_tags_of_its_own_block() {
        let source = "<div align=\"center\">\n<script>\n</div>\n\n本文\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![("本文", Some(TextAlign::Center)), ("後ろ", None)]
        );
    }

    /// The summary of a disclosure opened by an earlier block can sit in the
    /// closing block, after a blank line; parsing that block again behind a
    /// `<details>` must not take it as the stand-in disclosure's summary.
    #[test]
    fn test_closing_block_keeps_a_summary_of_its_own() {
        use html::html_parser::{ParsedHtmlElement, html_paragraph_text};

        let source = "<div align=\"center\">\n<details>\n\n<summary>題</summary>\n<p>本文</p>\n</details>\n</div>\n\n後ろ\n";
        let texts = html_block_children(source, "<summary")
            .iter()
            .map(|element| match element {
                ParsedHtmlElement::Paragraph(paragraph) => (
                    html_paragraph_text(&paragraph.contents),
                    paragraph.text_align,
                ),
                other => panic!("unexpected element {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                ("題".to_string(), Some(TextAlign::Center)),
                ("本文".to_string(), Some(TextAlign::Center)),
            ]
        );
    }

    /// The keys of every disclosure in the document, in order.
    fn details_keys(source: &str) -> Vec<html::html_parser::HtmlDetailsKey> {
        use html::html_parser::ParsedHtmlElement;

        fn collect(
            elements: &[ParsedHtmlElement],
            keys: &mut Vec<html::html_parser::HtmlDetailsKey>,
        ) {
            for element in elements {
                match element {
                    ParsedHtmlElement::Details(details) => {
                        keys.push(details.key);
                        collect(&details.children, keys);
                    }
                    ParsedHtmlElement::BlockQuote(block_quote) => {
                        collect(&block_quote.children, keys)
                    }
                    ParsedHtmlElement::List(list) => {
                        for item in &list.items {
                            collect(&item.content, keys);
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut keys = Vec::new();
        for block in parse_markdown_with_options(source, true, false, false)
            .html_blocks
            .values()
        {
            collect(&block.children, &mut keys);
        }
        keys
    }

    /// Aligning a closing block that also closes a disclosure must number the
    /// disclosures exactly as the same document without the alignment does,
    /// or their open states would move to other disclosures.
    #[test]
    fn test_closing_block_with_details_numbers_disclosures_as_without_alignment() {
        use html::html_parser::html_details_key;

        let aligned = "<details>\n<summary>題</summary>\n\n<div align=\"center\">\n\n本文\n\n<details><summary>題</summary>二</details>\n</div>\n</details>\n\n<details><summary>題</summary>三</details>\n";
        let plain = aligned.replace(" align=\"center\"", "");
        let expected = vec![
            html_details_key("題", 0),
            html_details_key("題", 1),
            html_details_key("題", 2),
        ];
        assert_eq!(details_keys(&plain), expected);
        assert_eq!(details_keys(aligned), expected);
    }

    #[test]
    fn test_aligned_div_does_not_leak_out_of_lists_and_quotes() {
        // Opened in an item of a list inside an aligned `<div>`.
        let source = "<div align=\"center\">\n\n- 一\n\n  <div align=\"right\">\n\n  右\n\n- 二\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("一", Some(TextAlign::Center)),
                ("右", Some(TextAlign::Right)),
                ("二", Some(TextAlign::Center)),
                ("後ろ", None),
            ]
        );

        // Opened in a nested item: the next nested item and the outer list's
        // next item are outside it.
        let source = "- 外\n\n  - <div align=\"right\">\n\n    内\n\n  - 次\n\n- 二\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("外", None),
                ("内", Some(TextAlign::Right)),
                ("次", None),
                ("二", None),
                ("後ろ", None),
            ]
        );

        // Closed from inside a block quote, so the `</div>` after the quote
        // closes nothing and the text between is outside.
        let source = "<div align=\"center\">\n\n> 引用\n>\n> </div>\n\n外\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("引用", Some(TextAlign::Center)),
                ("外", None),
                ("後ろ", None),
            ]
        );

        // Never closed after a list: nothing is aligned.
        let source = "<div align=\"center\">\n\n- 一\n\n- 二\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![("一", None), ("二", None), ("後ろ", None)]
        );
    }

    /// The HTML table of the block starting with `block_source`, with the
    /// alignment it moves by.
    fn html_block_table_alignment(source: &str, block_source: &str) -> Option<TextAlign> {
        use html::html_parser::ParsedHtmlElement;

        match html_block_children(source, block_source).as_slice() {
            [ParsedHtmlElement::Table(table)] => table.text_align,
            other => panic!("expected a table, got {other:?}"),
        }
    }

    #[test]
    fn test_aligned_div_moves_html_tables_as_a_whole() {
        let source = "<div align=\"center\">\n\n<table><tr><td>セル</td></tr></table>\n\n</div>\n";
        assert_eq!(
            html_block_table_alignment(source, "<table"),
            Some(TextAlign::Center)
        );

        let source = "<div align=\"right\"><table><tr><td>セル</td></tr></table></div>\n";
        assert_eq!(
            html_block_table_alignment(source, "<div"),
            Some(TextAlign::Right)
        );

        // The table's own `align` places it, which is not what this carries.
        let source = "<table align=\"center\"><tr><td>セル</td></tr></table>\n";
        assert_eq!(html_block_table_alignment(source, "<table"), None);
    }

    #[test]
    fn test_plain_div_inside_an_aligned_div_is_closed_first() {
        let source =
            "<div align=\"center\">\n\n<div>\n\n中\n\n</div>\n\nまだ中\n\n</div>\n\n後ろ\n";

        assert_eq!(
            aligned_texts(source),
            vec![
                ("中", Some(TextAlign::Center)),
                ("まだ中", Some(TextAlign::Center)),
                ("後ろ", None),
            ]
        );
    }

    #[test]
    fn test_center_tag_and_text_align_style_span_blocks_too() {
        let source = "<center>\n\n中\n\n</center>\n\n<div style=\"text-align: right\">\n\n右\n\n</div>\n\n後ろ\n";

        assert_eq!(
            aligned_texts(source),
            vec![
                ("中", Some(TextAlign::Center)),
                ("右", Some(TextAlign::Right)),
                ("後ろ", None),
            ]
        );
    }

    #[test]
    fn test_aligned_div_does_not_reach_across_a_list_item() {
        // Opened in a list item and closed after the list: a browser closes it
        // with the item, and the `</div>` after the list closes nothing.
        let source = "- <div align=\"center\">\n\n  中\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![("中", Some(TextAlign::Center)), ("後ろ", None)]
        );

        // Never closed at all: the item still bounds it.
        let source = "- <div align=\"right\">\n\n  中\n\n  中2\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("中", Some(TextAlign::Right)),
                ("中2", Some(TextAlign::Right)),
                ("後ろ", None)
            ]
        );

        // Opened outside the list and closed inside an item, which closes the
        // container from there, so the next item is outside it.
        let source = "<div align=\"center\">\n\n- 項目\n\n  </div>\n\n- 次\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("項目", Some(TextAlign::Center)),
                ("次", None),
                ("後ろ", None)
            ]
        );

        // Opened in one item and closed in the next, where it is already closed.
        let source = "- <div align=\"center\">\n\n  中\n\n- </div>\n\n  次\n";
        assert_eq!(
            aligned_texts(source),
            vec![("中", Some(TextAlign::Center)), ("次", None)]
        );

        // Opened and closed inside the same item.
        let source = "- 項目\n\n  <div align=\"right\">\n\n  右\n\n  </div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![
                ("項目", None),
                ("右", Some(TextAlign::Right)),
                ("後ろ", None)
            ]
        );
    }

    #[test]
    fn test_aligned_div_and_details_nest_either_way() {
        let source = "<div align=\"center\">\n\n<details>\n<summary>題</summary>\n\n本文\n\n</details>\n\n</div>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![("本文", Some(TextAlign::Center)), ("後ろ", None)]
        );
        let details_start = source.find("<details>").expect("the disclosure is there");
        assert!(
            parse_markdown_with_options(source, true, false, false)
                .html_blocks
                .get(&details_start)
                .is_some_and(|block| block.details_span.is_some()),
            "the disclosure still hides its body"
        );

        let source = "<details>\n<summary>題</summary>\n\n<div align=\"right\">\n\n本文\n\n</div>\n\n</details>\n\n後ろ\n";
        assert_eq!(
            aligned_texts(source),
            vec![("本文", Some(TextAlign::Right)), ("後ろ", None)]
        );
        assert!(
            parse_markdown_with_options(source, true, false, false)
                .html_blocks
                .get(&0)
                .is_some_and(|block| block.details_span.is_some())
        );

        // `</details>` closes the `<div>` opened inside it, so the `</div>`
        // after it is a stray one that must not pull the alignment further.
        let source = "<details>\n<summary>題</summary>\n\n<div align=\"right\">\n\n本文\n\n</details>\n\n後ろ\n\n</div>\n";
        assert_eq!(
            aligned_texts(source),
            vec![("本文", Some(TextAlign::Right)), ("後ろ", None)]
        );
    }

    #[test]
    fn test_aligned_div_pairs_only_real_container_tags() {
        let cases: [(&str, Vec<(&str, Option<TextAlign>)>); 7] = [
            // Opened in a block quote, which a browser closes with the quote.
            (
                "> <div align=\"center\">\n>\n> 中\n\n本文\n\n</div>\n\n後ろ\n",
                vec![
                    ("中", Some(TextAlign::Center)),
                    ("本文", None),
                    ("後ろ", None),
                ],
            ),
            // A `</div>` inside a comment or inline code closes nothing.
            (
                "<div align=\"center\" title=\"日本語の説明\">\n\n<!-- </div> -->\n\n`</div>`\n\n本文\n\n</div>\n\n後ろ\n",
                vec![("本文", Some(TextAlign::Center)), ("後ろ", None)],
            ),
            // One block closing a container and opening the next.
            (
                "<div align=\"center\">\n\nA\n\n</div>\n<div align=\"right\">\n\nB\n\n</div>\n\n後ろ\n",
                vec![
                    ("A", Some(TextAlign::Center)),
                    ("B", Some(TextAlign::Right)),
                    ("後ろ", None),
                ],
            ),
            // The outer container is never closed, so only the inner one aligns.
            (
                "<div align=\"center\">\n\n<div align=\"right\">\n\nA\n\n</div>\n\nB\n",
                vec![("A", Some(TextAlign::Right)), ("B", None)],
            ),
            // A `</div>` inside a paragraph is inline HTML, not a block.
            (
                "<div align=\"center\">\n\n本文 </div>\n\n後ろ\n",
                vec![("本文 ", None), ("後ろ", None)],
            ),
            (
                "<DIV ALIGN=\"CENTER\">\n\n本文\n\n</DIV>\n\n後ろ\n",
                vec![("本文", Some(TextAlign::Center)), ("後ろ", None)],
            ),
            // An opening tag cut off by the blank line never opens anything.
            (
                "<div align=center\n\n本文\n\n</div>\n\n後ろ\n",
                vec![("本文", None), ("後ろ", None)],
            ),
        ];

        for (source, expected) in cases {
            assert_eq!(aligned_texts(source), expected, "{source:?}");
        }
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

    /// Every span style the preview would draw, with the source it covers.
    fn span_styles(source: &str) -> Vec<(String, InlineSpanStyle)> {
        parse_markdown_with_options(source, true, false, false)
            .events
            .into_iter()
            .filter_map(|(range, event)| match event {
                MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Span(style)) => {
                    Some((source.get(range)?.to_string(), style))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_span_mark_and_font_colors_reach_the_renderer() {
        let source = "a <span style=\"color: #d44c47\">赤</span> <mark>黄</mark> \
            <mark style=\"background-color: #b7ebc6\">緑</mark> <font color=\"blue\">青</font> \
            <span style=\"background: yellow\">地</span> <span style='color: rgb(1, 2, 3)'>x</span>";
        assert_eq!(
            span_styles(source),
            vec![
                (
                    "<span style=\"color: #d44c47\">赤</span>".to_string(),
                    InlineSpanStyle {
                        color: Some(0xd44c47),
                        ..Default::default()
                    }
                ),
                (
                    "<mark>黄</mark>".to_string(),
                    InlineSpanStyle {
                        color: Some(0x000000),
                        background: Some(0xffff00),
                        ..Default::default()
                    }
                ),
                (
                    "<mark style=\"background-color: #b7ebc6\">緑</mark>".to_string(),
                    InlineSpanStyle {
                        color: Some(0x000000),
                        background: Some(0xb7ebc6),
                        ..Default::default()
                    }
                ),
                (
                    "<font color=\"blue\">青</font>".to_string(),
                    InlineSpanStyle {
                        color: Some(0x0000ff),
                        ..Default::default()
                    }
                ),
                (
                    "<span style=\"background: yellow\">地</span>".to_string(),
                    InlineSpanStyle {
                        background: Some(0xffff00),
                        ..Default::default()
                    }
                ),
                (
                    "<span style='color: rgb(1, 2, 3)'>x</span>".to_string(),
                    InlineSpanStyle {
                        color: Some(0x010203),
                        ..Default::default()
                    }
                ),
            ]
        );

        // The tags themselves are never drawn as text.
        let parsed = parse_markdown_with_options(source, true, false, false);
        assert!(
            !parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml)),
            "{:?}",
            parsed.events
        );
    }

    #[test]
    fn test_colors_the_preview_cannot_read_leave_the_text_plain() {
        for source in [
            "a <span style=\"color: nonsense\">x</span> b",
            "a <span style=\"color: color-mix(in srgb, red, blue)\">x</span> b",
            "a <span style=\"color: #12345\">x</span> b",
            "a <span class=\"note\">x</span> b",
            "a <font size=\"5\">x</font> b",
        ] {
            assert_eq!(span_styles(source), Vec::new(), "{source}");
            let parsed = parse_markdown_with_options(source, true, false, false);
            assert!(
                parsed.events.iter().any(|(range, event)| {
                    matches!(event, MarkdownEvent::Text) && source.get(range.clone()) == Some("x")
                }),
                "{source} should still show its text: {:?}",
                parsed.events
            );
        }
    }

    #[test]
    fn test_translucent_span_colors_reach_the_renderer_and_the_export() {
        let styles = span_styles(
            "a <span style=\"color: rgba(255, 0, 16, 0.5); background: hsl(0 100% 50%)\">x</span> <mark style=\"background-color: transparent\">y</mark>",
        );
        let [(_, translucent), (_, clear)] = styles.as_slice() else {
            panic!("expected two styled spans: {styles:?}");
        };
        let channels = html::html_parser::css_color_channels;
        let color = translucent.color.expect("the span has a color");
        assert_eq!(channels(color), [255, 0, 16, 128]);
        let background = translucent.background.expect("the span has a background");
        assert_eq!(background, 0xff0000);
        assert_eq!(channels(background), [255, 0, 0, 255]);
        let clear = clear.background.expect("the mark has a background");
        assert_eq!(channels(clear), [0, 0, 0, 0]);
    }

    #[test]
    fn test_nested_colors_pair_the_way_html_does() {
        // The plain span inside closes itself, so the outer color reaches the
        // end of the paragraph's last `</span>` and the inner one wins inside.
        let source = "<span style=\"color: red\">a <span>b</span> <span style=\"color: blue\">c</span> d</span>";
        let parsed = parse_markdown_with_options(source, true, false, false);
        let order = parsed
            .events
            .iter()
            .filter_map(|(range, event)| match event {
                MarkdownEvent::InlineHtmlStyleStart(InlineHtmlStyle::Span(style)) => {
                    Some(format!("start {:06x}", style.color.unwrap_or_default()))
                }
                MarkdownEvent::InlineHtmlStyleEnd(InlineHtmlStyle::Span(style)) => {
                    Some(format!("end {:06x}", style.color.unwrap_or_default()))
                }
                MarkdownEvent::Text => source.get(range.clone()).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                "start ff0000",
                "a ",
                "b",
                " ",
                "start 0000ff",
                "c",
                "end 0000ff",
                " d",
                "end ff0000"
            ]
        );
    }

    #[test]
    fn test_unmatched_color_tags_do_not_color_the_rest_of_the_document() {
        for source in [
            "<span style=\"color: red\">a\n\nb",
            "a</span> <span style=\"color: red\">b\n\nc</span>",
            "<mark>a\n\nb</mark>",
            "- <span style=\"color: red\">a\n- b</span>",
            "<span style=\"color: red\">a <mark>b</span> c</mark>",
        ] {
            let parsed = parse_markdown_with_options(source, true, false, false);
            let mut depth = 0isize;
            for (_, event) in &parsed.events {
                match event {
                    MarkdownEvent::InlineHtmlStyleStart(_) => depth += 1,
                    MarkdownEvent::InlineHtmlStyleEnd(_) => depth -= 1,
                    MarkdownEvent::End(MarkdownTagEnd::Paragraph | MarkdownTagEnd::Item) => {
                        assert_eq!(depth, 0, "{source}: a style outlived its block");
                    }
                    _ => {}
                }
            }
            assert_eq!(depth, 0, "{source}: unbalanced styles {:?}", parsed.events);
        }

        // A closing tag cannot end a paragraph other than its own.
        assert_eq!(
            span_styles("<span style=\"color: red\">a\n\nb</span>"),
            Vec::new()
        );
    }

    #[test]
    fn test_font_size_whole_paragraph_edges() {
        let scale = |source: &str| {
            span_styles(source)
                .into_iter()
                .map(|(_, style)| (style.paragraph_scale_percent, style.text_scale_percent))
                .collect::<Vec<_>>()
        };
        let large = "<span style=\"font-size: 1.25em\">";
        let whole = (Some(125), None);
        let part = (None, Some(125));

        // Whitespace around the paragraph is not part of its text.
        assert_eq!(scale(&format!("  {large}a</span>  ")), [whole]);
        assert_eq!(scale(&format!("{large}a\nb</span>")), [whole]);
        assert_eq!(scale(&format!("{large}a  \nb</span>")), [whole]);
        // Anything the span leaves out keeps the paragraph at its size, and
        // the span's text alone is drawn at the span's.
        assert_eq!(scale(&format!("{large}a</span><br>")), [part]);
        assert_eq!(scale(&format!("<br>{large}a</span>")), [part]);
        assert_eq!(scale(&format!("{large}a</span>\nb")), [part]);
        assert_eq!(scale(&format!("a\n{large}b</span>")), [part]);
        assert_eq!(
            scale(&format!("{large}a</span>{large}b</span>")),
            [part, part]
        );
        assert_eq!(scale(&format!("{large}a</span> `code`")), [part]);
        assert_eq!(scale(&format!("{large}a</span>[link](x)")), [part]);
        // A size inside another span that holds the whole paragraph is not the
        // paragraph's.
        assert_eq!(
            scale(&format!(
                "<span style=\"color: red\">{large}a</span> b</span>"
            )),
            [(None, None), part]
        );
    }

    #[test]
    fn test_a_stray_span_does_not_cost_the_element_around_it_its_style() {
        for source in [
            "<b>x <span>y</b> z",
            "<b>x <span style=\"color: red\">y</b> z",
            "<b>x <mark>y <font color=\"red\">w</b> z",
        ] {
            let parsed = parse_markdown_with_options(source, true, false, false);
            let strong = parsed
                .events
                .iter()
                .filter(|(_, event)| {
                    matches!(
                        event,
                        MarkdownEvent::Start(MarkdownTag::Strong)
                            | MarkdownEvent::End(MarkdownTagEnd::Strong)
                    )
                })
                .count();
            assert_eq!(strong, 2, "{source}: {:?}", parsed.events);
            assert_eq!(span_styles(source), Vec::new(), "{source}");
        }

        // Anything else left open inside still keeps the closing tag from
        // pairing, as before.
        let parsed = parse_markdown_with_options("<b>x <i>y</b> z", true, false, false);
        assert!(
            !parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Strong))),
            "{:?}",
            parsed.events
        );
    }

    #[test]
    fn test_font_size_applies_only_to_a_whole_paragraph() {
        let scale = |source: &str| {
            span_styles(source)
                .into_iter()
                .map(|(_, style)| style.paragraph_scale_percent)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            scale("<span style=\"font-size: 1.25em\">段落全体</span>"),
            [Some(125)]
        );
        assert_eq!(
            scale("> <span style=\"font-size: 1.5em\">引用\nの中</span>"),
            [Some(150)]
        );
        // A tight list item's text runs straight into its nested list.
        assert_eq!(
            scale("- <span style=\"font-size: 0.85em\">項目</span>\n  - 入れ子"),
            [Some(85)]
        );
        assert_eq!(
            scale("- [ ] <span style=\"font-size: 125%\">タスク</span>"),
            [Some(125)]
        );
        assert_eq!(
            scale("- a\n\n- <span style=\"font-size: 1.25em\">loose</span>"),
            [Some(125)]
        );

        // Part of a paragraph is drawn at its size within the paragraph's
        // lines instead.
        let text_scale = |source: &str| {
            span_styles(source)
                .into_iter()
                .map(|(_, style)| (style.paragraph_scale_percent, style.text_scale_percent))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            text_scale("a <span style=\"font-size: 1.25em\">b</span>"),
            [(None, Some(125))]
        );
        assert_eq!(
            text_scale("<span style=\"font-size: 1.25em\">a</span> b"),
            [(None, Some(125))]
        );
        assert_eq!(
            text_scale("- 項目の <span style=\"font-size: 24px\">一部</span>"),
            [(None, Some(150))]
        );
        // Headings are sized by their level, and a span that asks for nothing
        // else is dropped altogether.
        assert_eq!(
            scale("# <span style=\"font-size: 1.25em\">見出し</span>"),
            Vec::<Option<u16>>::new()
        );
        assert_eq!(
            scale("## 見出しの <span style=\"font-size: 1.25em\">一部</span>"),
            Vec::<Option<u16>>::new()
        );
        // A color on the same span in a heading survives without the size.
        assert_eq!(
            span_styles("# <span style=\"color: red; font-size: 1.25em\">a</span> b"),
            vec![(
                "<span style=\"color: red; font-size: 1.25em\">a</span>".to_string(),
                InlineSpanStyle {
                    color: Some(0xff0000),
                    ..Default::default()
                }
            )]
        );
    }
}
