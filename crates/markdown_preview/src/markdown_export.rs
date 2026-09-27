//! DESIGN.md §5.3 (ADR-F05): Markdown → PDF 書き出し。
//!
//! `markdown` クレートのイベント列から自己完結の HTML (CSS 埋め込み・外部リソース参照なし)
//! を組み立て、Edge の headless モードで PDF へ印刷する。Edge が見つからない場合は
//! PATH 上の pandoc へフォールバックする。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result, anyhow};
use fs::{Fs, RemoveOptions};
use markdown::parser::{
    Alignment, CodeBlockKind, HeadingLevel, MarkdownEvent, MarkdownTag, MarkdownTagEnd,
    extract_code_content_range, parse_markdown_events,
};
use util::command::new_command;

/// Edge/Chromium 系ブラウザーが `--print-to-pdf` に使う既定の用紙余白は狭いので、
/// `@page` で本文用の余白を指定する。
const DOCUMENT_CSS: &str = r#"
@page { size: A4; margin: 18mm 16mm; }
:root { color-scheme: light; }
html { -webkit-print-color-adjust: exact; print-color-adjust: exact; }
body {
  margin: 0;
  color: #1f2328;
  background: #ffffff;
  font-family: "Yu Gothic UI", "Meiryo", "Noto Sans JP", "Hiragino Kaku Gothic ProN", "Segoe UI", sans-serif;
  font-size: 10.5pt;
  line-height: 1.75;
  word-wrap: break-word;
}
h1, h2, h3, h4, h5, h6 { margin: 1.4em 0 0.6em; font-weight: 600; line-height: 1.35; page-break-after: avoid; }
h1 { font-size: 1.9em; border-bottom: 1px solid #d1d9e0; padding-bottom: 0.25em; }
h2 { font-size: 1.5em; border-bottom: 1px solid #d1d9e0; padding-bottom: 0.25em; }
h3 { font-size: 1.25em; }
h4 { font-size: 1.05em; }
h5 { font-size: 0.95em; }
h6 { font-size: 0.9em; color: #59636e; }
p { margin: 0 0 0.9em; }
a { color: #0969da; text-decoration: none; }
ul, ol { margin: 0 0 0.9em; padding-left: 2em; }
li { margin: 0.2em 0; }
li > input[type="checkbox"] { margin-right: 0.4em; }
blockquote {
  margin: 0 0 0.9em;
  padding: 0.1em 1em;
  color: #59636e;
  border-left: 0.25em solid #d1d9e0;
}
hr { height: 1px; margin: 1.6em 0; border: 0; background: #d1d9e0; }
code, kbd, samp {
  font-family: "Cascadia Mono", "Consolas", "BIZ UDGothic", "MS Gothic", monospace;
  font-size: 0.88em;
}
p code, li code, td code, th code, h1 code, h2 code, h3 code, h4 code, h5 code, h6 code {
  padding: 0.15em 0.35em;
  background: #eff1f3;
  border-radius: 4px;
}
pre {
  margin: 0 0 0.9em;
  padding: 0.8em 1em;
  overflow-x: auto;
  background: #f6f8fa;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-all;
}
pre code { padding: 0; background: none; }
table { margin: 0 0 0.9em; border-collapse: collapse; max-width: 100%; }
th, td { padding: 0.4em 0.8em; border: 1px solid #d1d9e0; }
th { background: #f6f8fa; font-weight: 600; }
img { max-width: 100%; }
figure.mermaid { margin: 0 0 0.9em; text-align: center; page-break-inside: avoid; }
figure.mermaid svg { max-width: 100%; height: auto; }
p.mermaid-error { margin: 0 0 0.3em; color: #953800; font-size: 0.9em; }
sup, sub { line-height: 0; }
.footnotes { margin-top: 2em; padding-top: 0.8em; border-top: 1px solid #d1d9e0; font-size: 0.9em; }
"#;

/// Renders `source` as a standalone HTML document with no external resources.
///
/// Relative link and image destinations are resolved against `base_directory`
/// and rewritten to `file://` URLs so a headless browser can load them.
pub fn render_html_document(
    source: &str,
    title: &str,
    base_directory: Option<&Path>,
    soft_break_as_hard_break: bool,
) -> String {
    let mut document = String::with_capacity(source.len() * 2 + DOCUMENT_CSS.len() + 512);
    document
        .push_str("<!DOCTYPE html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n<title>");
    escape_text(title, &mut document);
    document.push_str("</title>\n<style>");
    document.push_str(DOCUMENT_CSS);
    document.push_str("</style>\n</head>\n<body>\n");
    render_body(
        source,
        base_directory,
        soft_break_as_hard_break,
        &mut document,
    );
    document.push_str("</body>\n</html>\n");
    document
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    in_header: bool,
    column: usize,
}

/// The theme Mermaid diagrams are drawn with when printing.
///
/// It is a fixed light palette matching [`DOCUMENT_CSS`] rather than the user's
/// editor theme, because the PDF is printed on white paper. The font stack is
/// deliberately short: `merman` truncates the `style` attribute it writes onto
/// text elements, and a long stack loses its tail mid-entity, which makes the
/// SVG fail to parse.
pub(crate) fn print_mermaid_theme() -> mermaid_render::MermaidTheme {
    let git_branch_colors = [
        gpui::rgb(0x0969da).into(),
        gpui::rgb(0x1a7f37).into(),
        gpui::rgb(0x8250df).into(),
        gpui::rgb(0xbc4c00).into(),
        gpui::rgb(0x0f6cbd).into(),
        gpui::rgb(0x6e7781).into(),
        gpui::rgb(0xbf3989).into(),
        gpui::rgb(0xcf222e).into(),
    ];

    mermaid_render::MermaidTheme {
        dark_mode: false,
        font_family: "\"Yu Gothic UI\", \"Meiryo\", sans-serif".to_owned(),
        background: gpui::rgb(0xffffff).into(),
        primary_color: gpui::rgb(0xf6f8fa).into(),
        primary_text_color: gpui::rgb(0x1f2328).into(),
        primary_border_color: gpui::rgb(0x8c959f).into(),
        secondary_color: gpui::rgb(0xeff1f3).into(),
        tertiary_color: gpui::rgb(0xffffff).into(),
        line_color: gpui::rgb(0x59636e).into(),
        text_color: gpui::rgb(0x1f2328).into(),
        edge_label_background: gpui::rgb(0xffffff).into(),
        cluster_background: gpui::rgb(0xf6f8fa).into(),
        cluster_border: gpui::rgb(0xd1d9e0).into(),
        note_background: gpui::rgb(0xfff8c5).into(),
        note_border: gpui::rgb(0xd4a72c).into(),
        actor_background: gpui::rgb(0xf6f8fa).into(),
        actor_border: gpui::rgb(0x8c959f).into(),
        activation_background: gpui::rgb(0xeff1f3).into(),
        activation_border: gpui::rgb(0x8c959f).into(),
        git_branch_colors,
        git_branch_label_colors: git_branch_colors.map(mermaid_render::text_color_for_background),
        er_attr_bg_odd: gpui::rgb(0xf6f8fa).into(),
        er_attr_bg_even: gpui::rgb(0xeff1f3).into(),
        error_color: gpui::rgb(0xcf222e).into(),
        warning_color: gpui::rgb(0x9a6700).into(),
        accent_colors: Vec::new(),
    }
}

/// The SVG for a fenced mermaid block, or the reason it could not be drawn.
fn render_mermaid_block(contents: &str) -> Result<String, String> {
    if !markdown::is_supported_diagram_type(contents) {
        return Err("未対応の図の種類です".to_owned());
    }

    mermaid_render::render_to_svg(contents, &print_mermaid_theme())
        .map_err(|error| format!("{error:#}"))
}

fn render_body(
    source: &str,
    base_directory: Option<&Path>,
    soft_break_as_hard_break: bool,
    output: &mut String,
) {
    let events = parse_markdown_events(source);

    // Image alt text arrives as the inline events between `Start(Image)` and
    // `End(Image)`, so the `<img>` tag can only be emitted once the end tag is
    // seen; until then inline output is diverted into `image_alt`.
    let mut image: Option<(String, String)> = None;
    let mut image_alt = String::new();
    let mut metadata_depth = 0usize;
    let mut table: Option<TableState> = None;
    let mut in_footnotes = false;
    // Set while the events of a mermaid block that was replaced by its drawing
    // are being skipped, so the source is not printed underneath the diagram.
    let mut in_drawn_mermaid_block = false;

    for (range, event) in events {
        if in_drawn_mermaid_block {
            if matches!(event, MarkdownEvent::End(MarkdownTagEnd::CodeBlock)) {
                in_drawn_mermaid_block = false;
            }
            continue;
        }

        if metadata_depth > 0 {
            match event {
                MarkdownEvent::Start(_) => metadata_depth += 1,
                MarkdownEvent::End(MarkdownTagEnd::MetadataBlock(_)) => metadata_depth -= 1,
                MarkdownEvent::End(_) => metadata_depth -= 1,
                _ => {}
            }
            continue;
        }

        // Alt text is plain text, so inline markup inside an image is dropped
        // rather than being emitted around the `<img>` tag.
        if image.is_some()
            && !matches!(
                event,
                MarkdownEvent::Text
                    | MarkdownEvent::SubstitutedText(_)
                    | MarkdownEvent::Code
                    | MarkdownEvent::SubstitutedCode(_)
                    | MarkdownEvent::SoftBreak
                    | MarkdownEvent::End(MarkdownTagEnd::Image)
            )
        {
            continue;
        }

        match event {
            MarkdownEvent::RootStart | MarkdownEvent::RootEnd(_) => {}
            MarkdownEvent::Start(tag) => match tag {
                MarkdownTag::Paragraph => output.push_str("<p>"),
                MarkdownTag::Heading { level, id, .. } => {
                    output.push('<');
                    output.push_str(heading_tag(level));
                    if let Some(id) = id {
                        output.push_str(" id=\"");
                        escape_attribute(&id, output);
                        output.push('"');
                    }
                    output.push('>');
                }
                MarkdownTag::BlockQuote(_) => output.push_str("<blockquote>\n"),
                MarkdownTag::CodeBlock { kind, metadata } => {
                    // A mermaid block becomes the diagram it describes; when it
                    // cannot be drawn the source is printed as usual, preceded
                    // by the reason so the reader is not left guessing.
                    let mermaid_contents = match &kind {
                        CodeBlockKind::FencedLang(info)
                            if metadata.is_fenced_closed
                                && markdown::parse_mermaid_info(info).is_some() =>
                        {
                            source.get(metadata.content_range.clone())
                        }
                        _ => None,
                    };
                    if let Some(contents) = mermaid_contents {
                        match render_mermaid_block(contents.trim_end_matches('\n')) {
                            Ok(svg) => {
                                output.push_str("<figure class=\"mermaid\">");
                                output.push_str(&svg);
                                output.push_str("</figure>\n");
                                in_drawn_mermaid_block = true;
                                continue;
                            }
                            Err(reason) => {
                                output.push_str("<p class=\"mermaid-error\">");
                                escape_text("Mermaid 図を描画できませんでした: ", output);
                                escape_text(&reason, output);
                                output.push_str("</p>\n");
                            }
                        }
                    }

                    output.push_str("<pre><code");
                    if let CodeBlockKind::FencedLang(language) = &kind {
                        output.push_str(" class=\"language-");
                        escape_attribute(language, output);
                        output.push('"');
                    }
                    output.push('>');
                }
                MarkdownTag::HtmlBlock => {}
                MarkdownTag::List(first_item) => match first_item {
                    Some(start) => {
                        let _ = write!(output, "<ol start=\"{start}\">\n");
                    }
                    None => output.push_str("<ul>\n"),
                },
                MarkdownTag::Item => output.push_str("<li>"),
                MarkdownTag::FootnoteDefinition(label) => {
                    if !in_footnotes {
                        in_footnotes = true;
                        output.push_str("<div class=\"footnotes\">\n");
                    }
                    output.push_str("<div id=\"fn-");
                    escape_attribute(&label, output);
                    output.push_str("\"><sup>");
                    escape_text(&label, output);
                    output.push_str("</sup> ");
                }
                MarkdownTag::Table(alignments) => {
                    table = Some(TableState {
                        alignments,
                        in_header: false,
                        column: 0,
                    });
                    output.push_str("<table>\n");
                }
                MarkdownTag::TableHead => {
                    if let Some(table) = table.as_mut() {
                        table.in_header = true;
                        table.column = 0;
                    }
                    output.push_str("<thead><tr>");
                }
                MarkdownTag::TableRow => {
                    if let Some(table) = table.as_mut() {
                        table.column = 0;
                    }
                    output.push_str("<tr>");
                }
                MarkdownTag::TableCell => {
                    let (cell_tag, alignment) = match table.as_ref() {
                        Some(table) => (
                            if table.in_header { "th" } else { "td" },
                            table.alignments.get(table.column).copied(),
                        ),
                        None => ("td", None),
                    };
                    output.push('<');
                    output.push_str(cell_tag);
                    match alignment {
                        Some(Alignment::Left) => output.push_str(" style=\"text-align:left\""),
                        Some(Alignment::Center) => output.push_str(" style=\"text-align:center\""),
                        Some(Alignment::Right) => output.push_str(" style=\"text-align:right\""),
                        Some(Alignment::None) | None => {}
                    }
                    output.push('>');
                }
                MarkdownTag::Emphasis => output.push_str("<em>"),
                MarkdownTag::Strong => output.push_str("<strong>"),
                MarkdownTag::Strikethrough => output.push_str("<del>"),
                MarkdownTag::Superscript => output.push_str("<sup>"),
                MarkdownTag::Subscript => output.push_str("<sub>"),
                MarkdownTag::Link {
                    dest_url, title, ..
                } => {
                    output.push_str("<a href=\"");
                    escape_attribute(&resolve_url(&dest_url, base_directory), output);
                    output.push('"');
                    if !title.is_empty() {
                        output.push_str(" title=\"");
                        escape_attribute(&title, output);
                        output.push('"');
                    }
                    output.push('>');
                }
                MarkdownTag::Image {
                    dest_url, title, ..
                } => {
                    image_alt.clear();
                    image = Some((
                        resolve_url(&dest_url, base_directory),
                        title.as_ref().to_owned(),
                    ));
                }
                MarkdownTag::MetadataBlock(_) => metadata_depth = 1,
                MarkdownTag::DefinitionList => output.push_str("<dl>\n"),
                MarkdownTag::DefinitionListTitle => output.push_str("<dt>"),
                MarkdownTag::DefinitionListDefinition => output.push_str("<dd>"),
            },
            MarkdownEvent::End(tag) => match tag {
                MarkdownTagEnd::Paragraph => output.push_str("</p>\n"),
                MarkdownTagEnd::Heading(level) => {
                    output.push_str("</");
                    output.push_str(heading_tag(level));
                    output.push_str(">\n");
                }
                MarkdownTagEnd::BlockQuote(_) => output.push_str("</blockquote>\n"),
                MarkdownTagEnd::CodeBlock => output.push_str("</code></pre>\n"),
                MarkdownTagEnd::HtmlBlock => {}
                MarkdownTagEnd::List(ordered) => {
                    output.push_str(if ordered { "</ol>\n" } else { "</ul>\n" });
                }
                MarkdownTagEnd::Item => output.push_str("</li>\n"),
                MarkdownTagEnd::FootnoteDefinition => output.push_str("</div>\n"),
                MarkdownTagEnd::Table => {
                    table = None;
                    output.push_str("</tbody></table>\n");
                }
                MarkdownTagEnd::TableHead => {
                    if let Some(table) = table.as_mut() {
                        table.in_header = false;
                    }
                    output.push_str("</tr></thead><tbody>\n");
                }
                MarkdownTagEnd::TableRow => output.push_str("</tr>\n"),
                MarkdownTagEnd::TableCell => {
                    let cell_tag = match table.as_ref() {
                        Some(table) if table.in_header => "th",
                        _ => "td",
                    };
                    output.push_str("</");
                    output.push_str(cell_tag);
                    output.push('>');
                    if let Some(table) = table.as_mut() {
                        table.column += 1;
                    }
                }
                MarkdownTagEnd::Emphasis => output.push_str("</em>"),
                MarkdownTagEnd::Strong => output.push_str("</strong>"),
                MarkdownTagEnd::Strikethrough => output.push_str("</del>"),
                MarkdownTagEnd::Superscript => output.push_str("</sup>"),
                MarkdownTagEnd::Subscript => output.push_str("</sub>"),
                MarkdownTagEnd::Link => output.push_str("</a>"),
                MarkdownTagEnd::Image => {
                    if let Some((destination, title)) = image.take() {
                        output.push_str("<img src=\"");
                        escape_attribute(&destination, output);
                        output.push_str("\" alt=\"");
                        escape_attribute(&image_alt, output);
                        output.push('"');
                        if !title.is_empty() {
                            output.push_str(" title=\"");
                            escape_attribute(&title, output);
                            output.push('"');
                        }
                        output.push('>');
                        image_alt.clear();
                    }
                }
                MarkdownTagEnd::MetadataBlock(_) => {}
                MarkdownTagEnd::DefinitionList => output.push_str("</dl>\n"),
                MarkdownTagEnd::DefinitionListTitle => output.push_str("</dt>\n"),
                MarkdownTagEnd::DefinitionListDefinition => output.push_str("</dd>\n"),
            },
            // While collecting alt text the raw string is accumulated, because
            // it is escaped once when the attribute is written.
            MarkdownEvent::Text => {
                let text = source.get(range).unwrap_or_default();
                if image.is_some() {
                    image_alt.push_str(text);
                } else {
                    escape_text(text, output);
                }
            }
            MarkdownEvent::SubstitutedText(text) => {
                if image.is_some() {
                    image_alt.push_str(&text);
                } else {
                    escape_text(&text, output);
                }
            }
            MarkdownEvent::Code => {
                let span = source.get(range).unwrap_or_default();
                let content = span
                    .get(extract_code_content_range(span))
                    .unwrap_or_default();
                if image.is_some() {
                    image_alt.push_str(content);
                } else {
                    output.push_str("<code>");
                    escape_text(content, output);
                    output.push_str("</code>");
                }
            }
            MarkdownEvent::SubstitutedCode(text) => {
                if image.is_some() {
                    image_alt.push_str(&text);
                } else {
                    output.push_str("<code>");
                    escape_text(&text, output);
                    output.push_str("</code>");
                }
            }
            // Raw HTML authored in the document is passed through, matching how
            // Markdown renderers (and the in-editor preview) treat it.
            MarkdownEvent::Html | MarkdownEvent::InlineHtml => {
                output.push_str(source.get(range).unwrap_or_default());
            }
            // `parse_markdown_events` leaves inline HTML raw, so the tags reach
            // the arm above as written, and the browser that prints the page
            // draws their colors and sizes itself.
            MarkdownEvent::InlineHtmlStyleStart(_) | MarkdownEvent::InlineHtmlStyleEnd(_) => {}
            MarkdownEvent::FootnoteReference(label) => {
                output.push_str("<sup><a href=\"#fn-");
                escape_attribute(&label, output);
                output.push_str("\">");
                escape_text(&label, output);
                output.push_str("</a></sup>");
            }
            MarkdownEvent::SoftBreak => {
                if image.is_some() {
                    image_alt.push(' ');
                } else if soft_break_as_hard_break {
                    output.push_str("<br>\n");
                } else {
                    output.push('\n');
                }
            }
            MarkdownEvent::HardBreak => output.push_str("<br>\n"),
            MarkdownEvent::Rule => output.push_str("<hr>\n"),
            MarkdownEvent::TaskListMarker(checked) => {
                output.push_str(if checked {
                    "<input type=\"checkbox\" checked disabled>"
                } else {
                    "<input type=\"checkbox\" disabled>"
                });
            }
        }
    }

    if in_footnotes {
        output.push_str("</div>\n");
    }
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

fn escape_text(text: &str, output: &mut String) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ => output.push(character),
        }
    }
}

fn escape_attribute(text: &str, output: &mut String) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(character),
        }
    }
}

fn has_url_scheme(destination: &str) -> bool {
    let Some(colon) = destination.find(':') else {
        return false;
    };
    // A single leading letter is a Windows drive, not a scheme.
    if colon < 2 {
        return false;
    }
    let mut characters = destination[..colon].chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

/// Rewrites a relative destination into a `file://` URL so a headless browser
/// can resolve it; absolute URLs and in-document anchors are left alone.
fn resolve_url(destination: &str, base_directory: Option<&Path>) -> String {
    if destination.is_empty() || destination.starts_with('#') || has_url_scheme(destination) {
        return destination.to_owned();
    }
    let Some(base_directory) = base_directory else {
        return destination.to_owned();
    };

    let (path, fragment) = match destination.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (destination, None),
    };
    if path.is_empty() {
        return destination.to_owned();
    }

    let mut url = file_url(&base_directory.join(path));
    if let Some(fragment) = fragment {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for segment in path.to_string_lossy().replace('\\', "/").split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        url.push('/');
        if is_windows_drive(segment) {
            url.push_str(segment);
        } else {
            url.push_str(&urlencoding::encode(segment));
        }
    }
    url
}

fn is_windows_drive(segment: &str) -> bool {
    let mut characters = segment.chars();
    matches!(
        (characters.next(), characters.next(), characters.next()),
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic()
    )
}

/// Where the exported PDF for `markdown_path` is written: alongside the source,
/// with the extension replaced.
pub fn pdf_output_path(markdown_path: &Path) -> PathBuf {
    markdown_path.with_extension("pdf")
}

fn temp_html_path(markdown_path: &Path) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let stem = markdown_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_owned());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    paths::temp_dir().join(format!(
        "zed-markdown-export-{}-{sequence}-{stem}.html",
        std::process::id()
    ))
}

/// Known install locations for Edge, in the order Windows itself resolves them
/// (per-machine 64-bit, per-machine 32-bit, then per-user).
fn edge_candidate_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for (variable, suffix) in [
        (
            "ProgramFiles(x86)",
            r"Microsoft\Edge\Application\msedge.exe",
        ),
        ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
        ("ProgramW6432", r"Microsoft\Edge\Application\msedge.exe"),
        ("LOCALAPPDATA", r"Microsoft\Edge\Application\msedge.exe"),
    ] {
        if let Some(root) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(root).join(suffix));
        }
    }
    candidates
}

fn find_edge() -> Option<PathBuf> {
    edge_candidate_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| which::which("msedge").ok())
}

fn find_pandoc() -> Option<PathBuf> {
    which::which("pandoc").ok()
}

/// Converts `html_path` to `output_path` with headless Edge, falling back to
/// pandoc when Edge is not installed.
///
/// Returns a stable key describing why no converter could be found so the
/// caller can localize the message.
pub async fn print_html_to_pdf(fs: &dyn Fs, html_path: &Path, output_path: &Path) -> Result<()> {
    if let Some(edge) = find_edge() {
        // A dedicated profile directory keeps the headless run from colliding
        // with an interactive Edge session that already holds the default one.
        let profile_directory = html_path.with_extension("profile");
        let mut command = new_command(&edge);
        command.args([
            "--headless".into(),
            "--disable-gpu".into(),
            "--no-pdf-header-footer".into(),
            format!("--user-data-dir={}", profile_directory.display()),
            format!("--print-to-pdf={}", output_path.display()),
            file_url(html_path),
        ]);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to run {}", edge.display()))?;
        if let Err(error) = fs
            .remove_dir(
                &profile_directory,
                RemoveOptions {
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            )
            .await
        {
            log::warn!(
                "failed to remove temporary Edge profile {}: {error}",
                profile_directory.display()
            );
        }
        anyhow::ensure!(
            output.status.success(),
            "{} exited with {:?}: {}",
            edge.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        anyhow::ensure!(
            output_path.is_file(),
            "{} reported success but wrote no file",
            edge.display()
        );
        return Ok(());
    }

    if let Some(pandoc) = find_pandoc() {
        let mut command = new_command(&pandoc);
        command.args([
            html_path.as_os_str().to_owned(),
            "--from=html".into(),
            "--standalone".into(),
            "-o".into(),
            output_path.as_os_str().to_owned(),
        ]);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to run {}", pandoc.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "{} exited with {:?}: {}",
            pandoc.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Ok(());
    }

    Err(anyhow!(NO_CONVERTER))
}

/// Marker carried by the error returned when neither Edge nor pandoc is
/// installed, so the caller can substitute a localized message.
pub const NO_CONVERTER: &str = "no-pdf-converter";

/// Writes `source` as HTML into the temp directory and prints it to
/// `pdf_output_path(markdown_path)`, returning the PDF path.
pub async fn export_to_pdf(
    fs: &dyn Fs,
    markdown_path: &Path,
    source: &str,
    soft_break_as_hard_break: bool,
) -> Result<PathBuf> {
    let title = markdown_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Markdown".to_owned());
    let html = render_html_document(
        source,
        &title,
        markdown_path.parent(),
        soft_break_as_hard_break,
    );

    let html_path = temp_html_path(markdown_path);
    fs.write(&html_path, html.as_bytes())
        .await
        .with_context(|| format!("failed to write {}", html_path.display()))?;

    let output_path = pdf_output_path(markdown_path);
    let result = print_html_to_pdf(fs, &html_path, &output_path).await;

    if let Err(error) = fs
        .remove_file(
            &html_path,
            RemoveOptions {
                recursive: false,
                ignore_if_not_exists: true,
            },
        )
        .await
    {
        log::warn!(
            "failed to remove temporary HTML {}: {error}",
            html_path.display()
        );
    }

    result.map(|()| output_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str) -> String {
        let mut html = String::new();
        render_body(source, None, false, &mut html);
        html
    }

    fn render_with_soft_breaks_as_hard_breaks(source: &str) -> String {
        let mut html = String::new();
        render_body(source, None, true, &mut html);
        html
    }

    /// The printed document is laid out by a browser, which puts the markdown
    /// written between a table's blocks into the cell it lands in, the way
    /// GitHub does, as long as it is printed between the cell's tags.
    #[test]
    fn test_markdown_between_table_blocks_is_printed_inside_its_cell() {
        let html = render(
            "<table>\n<tr>\n<td>\n\n**左の列** の本文\n\n- 項目\n\n</td>\n<td>\n\n![](right.png)\n\n</td>\n</tr>\n</table>\n\n後ろ\n",
        );

        let positions = [
            "<table>",
            "<td>",
            "<strong>左の列</strong>",
            "<li>項目</li>",
            "</td>\n<td>",
            "<img src=\"right.png\"",
            "</td>\n</tr>\n</table>",
            "<p>後ろ</p>",
        ]
        .map(|fragment| {
            html.find(fragment)
                .unwrap_or_else(|| panic!("{fragment:?} should be printed: {html}"))
        });
        assert!(
            positions.is_sorted(),
            "the cells' markdown should be printed inside them: {html}"
        );
    }

    #[test]
    fn test_headings_and_paragraphs() {
        assert_eq!(
            render("# 見出し\n\n本文の & <タグ>\n"),
            "<h1>見出し</h1>\n<p>本文の &amp; &lt;タグ&gt;</p>\n"
        );
        assert_eq!(render("###### h6\n"), "<h6>h6</h6>\n");
    }

    #[test]
    fn test_inline_marks() {
        assert_eq!(
            render("*a* **b** ~~c~~ `d`\n"),
            "<p><em>a</em> <strong>b</strong> <del>c</del> <code>d</code></p>\n"
        );
    }

    #[test]
    fn test_lists() {
        assert_eq!(
            render("- one\n- two\n"),
            "<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"
        );
        assert_eq!(
            render("3. three\n4. four\n"),
            "<ol start=\"3\">\n<li>three</li>\n<li>four</li>\n</ol>\n"
        );
        assert!(render("- [x] done\n").contains("<input type=\"checkbox\" checked disabled>"));
    }

    #[test]
    fn test_code_block_keeps_language_and_escapes() {
        assert_eq!(
            render("```rust\nfn main() { if a < b {} }\n```\n"),
            "<pre><code class=\"language-rust\">fn main() { if a &lt; b {} }\n</code></pre>\n"
        );
    }

    #[test]
    fn test_mermaid_block_becomes_an_inline_diagram() {
        let html = render("```mermaid\nflowchart TD\n    A[開始] --> B[終了]\n```\n");
        assert!(html.starts_with("<figure class=\"mermaid\"><svg"), "{html}");
        assert!(html.trim_end().ends_with("</figure>"), "{html}");
        // The source is replaced by the drawing rather than printed twice.
        assert!(!html.contains("<pre>"), "{html}");
        assert!(!html.contains("flowchart TD"), "{html}");
    }

    #[test]
    fn test_undrawable_mermaid_block_falls_back_to_code() {
        let broken = render("```mermaid\nflowchart TD\n    A --> --> B\n```\n");
        assert!(
            broken.starts_with("<p class=\"mermaid-error\">Mermaid 図を描画できませんでした: "),
            "{broken}"
        );
        assert!(
            broken.contains("<pre><code class=\"language-mermaid\">flowchart TD\n"),
            "{broken}"
        );
        assert!(!broken.contains("<svg"), "{broken}");

        let unsupported = render("```mermaid\nzenuml\n    A->B: x\n```\n");
        assert!(
            unsupported.starts_with(
                "<p class=\"mermaid-error\">Mermaid 図を描画できませんでした: 未対応の図の種類です</p>\n"
            ),
            "{unsupported}"
        );
        assert!(
            unsupported.contains("<pre><code class=\"language-mermaid\">"),
            "{unsupported}"
        );

        // An unterminated fence is left alone, since its contents are still
        // being typed.
        let unclosed = render("```mermaid\nflowchart TD\n");
        assert!(!unclosed.contains("<svg"), "{unclosed}");
        assert!(!unclosed.contains("mermaid-error"), "{unclosed}");
    }

    /// A PDF is printed once and read on paper, so a disclosure written by
    /// `markdown::InsertDetails` has to reach it with its body intact rather
    /// than collapsed away.
    #[test]
    fn test_details_keeps_its_body_in_the_printed_document() {
        let html = render("<details><summary>開く</summary>\n\n```\n本文\n```\n</details>\n");
        assert!(html.contains("<details><summary>開く</summary>"), "{html}");
        assert!(html.contains("<pre><code>本文\n</code></pre>"), "{html}");
        assert!(html.contains("</details>"), "{html}");
    }

    #[test]
    fn test_blockquote_and_rule() {
        assert_eq!(
            render("> quoted\n"),
            "<blockquote>\n<p>quoted</p>\n</blockquote>\n"
        );
        assert_eq!(render("---\n"), "<hr>\n");
    }

    #[test]
    fn test_table_alignment() {
        let html = render("| a | b |\n|:--|--:|\n| 1 | 2 |\n");
        assert!(
            html.contains("<th style=\"text-align:left\">a</th>"),
            "{html}"
        );
        assert!(
            html.contains("<th style=\"text-align:right\">b</th>"),
            "{html}"
        );
        assert!(
            html.contains("<td style=\"text-align:left\">1</td>"),
            "{html}"
        );
        assert!(html.contains("</tbody></table>"), "{html}");
    }

    #[test]
    fn test_links_and_images_resolve_relative_paths() {
        let base = Path::new("/docs/guide");
        let mut html = String::new();
        render_body(
            "[説明](sub/page.md) と ![図](<img/a b.png>)",
            Some(base),
            false,
            &mut html,
        );
        assert!(
            html.contains("href=\"file:///docs/guide/sub/page.md\""),
            "{html}"
        );
        assert!(
            html.contains("src=\"file:///docs/guide/img/a%20b.png\""),
            "{html}"
        );
        assert!(html.contains("alt=\"図\""), "{html}");
    }

    #[test]
    fn test_image_alt_text_is_escaped_exactly_once() {
        assert_eq!(
            render("![a & b < c](x.png)"),
            "<p><img src=\"x.png\" alt=\"a &amp; b &lt; c\"></p>\n"
        );
        // Inline markup inside the alt is dropped rather than emitted around
        // the tag.
        assert_eq!(
            render("![*em* `code`](x.png)"),
            "<p><img src=\"x.png\" alt=\"em code\"></p>\n"
        );
    }

    #[test]
    fn test_absolute_urls_and_anchors_are_untouched() {
        let base = Path::new("/docs");
        let mut html = String::new();
        render_body(
            "[a](https://example.com/x) [b](#section) [c](mailto:a@example.com)",
            Some(base),
            false,
            &mut html,
        );
        assert!(html.contains("href=\"https://example.com/x\""), "{html}");
        assert!(html.contains("href=\"#section\""), "{html}");
        assert!(html.contains("href=\"mailto:a@example.com\""), "{html}");
    }

    #[test]
    fn test_windows_paths_become_file_urls() {
        assert_eq!(file_url(Path::new(r"I:\docs\a.md")), "file:///I:/docs/a.md");
        assert!(!has_url_scheme(r"C:\docs\a.md"));
        assert!(has_url_scheme("https://example.com"));
    }

    #[test]
    fn test_front_matter_is_skipped() {
        assert_eq!(render("---\ntitle: x\n---\n\n本文\n"), "<p>本文</p>\n");
    }

    #[test]
    fn test_raw_html_passes_through() {
        assert_eq!(render("<div>x</div>\n"), "<div>x</div>\n");
        assert!(render("a <br> b\n").contains("<br>"));
    }

    /// What the format bar writes reaches the printed page as written, so the
    /// browser that prints it draws the colors, markers and sizes itself.
    #[test]
    fn test_text_colors_markers_and_sizes_are_printed() {
        assert_eq!(
            render(
                "<span style=\"font-size: 1.25em\">a <span style=\"color: #d44c47\">赤</span> <mark>黄</mark> <mark style=\"background-color: #b7ebc6\">緑</mark></span>\n"
            ),
            "<p><span style=\"font-size: 1.25em\">a <span style=\"color: #d44c47\">赤</span> <mark>黄</mark> <mark style=\"background-color: #b7ebc6\">緑</mark></span></p>\n"
        );
        // So do the colors the preview reads in other spellings, and a size on
        // part of a paragraph.
        assert_eq!(
            render(
                "a <span style=\"font-size: 20px; color: rgba(255 0 0 / 50%)\">一部</span> <u>下線</u> b\n"
            ),
            "<p>a <span style=\"font-size: 20px; color: rgba(255 0 0 / 50%)\">一部</span> <u>下線</u> b</p>\n"
        );
    }

    /// The printed document is laid out by a browser, which aligns whatever
    /// lies between a `<div align>` block and its `</div>` block by itself, the
    /// way GitHub does, as long as the markdown lands between the two tags.
    #[test]
    fn test_aligned_div_across_blocks_encloses_its_markdown_body() {
        let html = render(
            "<div align=\"center\">\n\n# プロジェクト名\n\n<img src=\"logo.png\" width=\"200\">\n\n説明文\n\n- 項目\n\n</div>\n\n後ろ\n",
        );

        let positions = [
            "<div align=\"center\">",
            "<h1>プロジェクト名</h1>",
            "<img src=\"logo.png\" width=\"200\">",
            "<p>説明文</p>",
            "<li>項目</li>",
            "</div>",
            "<p>後ろ</p>",
        ]
        .map(|fragment| {
            html.find(fragment)
                .unwrap_or_else(|| panic!("{fragment:?} should be printed: {html}"))
        });
        assert!(
            positions.is_sorted(),
            "the body should be printed inside the div: {html}"
        );
    }

    /// The preview aligns a table column without an alignment of its own the
    /// way an enclosing `<div align>` asks, which the printed document leaves
    /// to the browser, so such a cell must not carry an alignment of its own.
    #[test]
    fn test_table_cells_without_alignment_are_left_to_an_enclosing_div() {
        let html =
            render("<div align=\"right\">\n\n| 列 | 中 |\n|---|:-:|\n| 値 | 真ん中 |\n\n</div>\n");

        let positions = [
            "<div align=\"right\">",
            "<td>値</td>",
            "<td style=\"text-align:center\">真ん中</td>",
            "</div>",
        ]
        .map(|fragment| {
            html.find(fragment)
                .unwrap_or_else(|| panic!("{fragment:?} should be printed: {html}"))
        });
        assert!(
            positions.is_sorted(),
            "the table should be printed inside the div: {html}"
        );
    }

    #[test]
    fn test_soft_break_follows_the_preview_setting() {
        assert_eq!(render("一行目\n二行目\n"), "<p>一行目\n二行目</p>\n");
        assert_eq!(
            render_with_soft_breaks_as_hard_breaks("一行目\n二行目\n"),
            "<p>一行目<br>\n二行目</p>\n",
            "the PDF has to break where the preview breaks"
        );
        assert_eq!(
            render_with_soft_breaks_as_hard_breaks("![alt\n続き](a.png)\n"),
            render("![alt\n続き](a.png)\n"),
            "a newline inside image alt text stays a space either way"
        );
    }

    #[test]
    fn test_document_is_self_contained() {
        let document = render_html_document("# タイトル\n", "doc.md", None, true);
        assert!(document.starts_with("<!DOCTYPE html>"));
        assert!(document.contains("<title>doc.md</title>"));
        assert!(document.contains("<style>"));
        assert!(!document.contains("http://"), "{document}");
        assert!(document.contains("<h1>タイトル</h1>"));
    }

    #[test]
    fn test_pdf_output_path_replaces_extension() {
        assert_eq!(
            pdf_output_path(Path::new("/docs/a.md")),
            PathBuf::from("/docs/a.pdf")
        );
    }
}
