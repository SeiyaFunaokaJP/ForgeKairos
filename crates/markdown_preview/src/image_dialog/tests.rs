use super::*;
use fs::FakeFs;
use gpui::{TestAppContext, VisualTestContext};
use serde_json::json;
use util::path;
use util::rel_path::{RelPath, rel_path};
use util::test::marked_text_ranges;
use workspace::MultiWorkspace;

/// A 3 × 2 PNG, so the size the dialog reports can be told apart from its
/// transpose.
const PNG_3_BY_2: [u8; 73] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0x12, 0x16, 0xf1,
    0x4d, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x41, 0x0c, 0x70, 0x16, 0x00, 0x41, 0xd2, 0x05, 0xfb, 0x87, 0xf0, 0xb9, 0x48, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn image(destination: &str, alt: &str) -> ImageMarkup {
    ImageMarkup {
        destination: destination.to_string(),
        alt: alt.to_string(),
        ..ImageMarkup::default()
    }
}

fn inline(text: &str) -> RenderedImage {
    RenderedImage {
        text: text.to_string(),
        is_block: false,
    }
}

fn block(text: &str) -> RenderedImage {
    RenderedImage {
        text: text.to_string(),
        is_block: true,
    }
}

/// The image the whole of `text` holds, read with the cursor in its middle.
fn parse_whole(text: &str) -> ImageMarkup {
    let middle = text.len() / 2;
    let middle = (0..=middle)
        .rev()
        .find(|offset| text.is_char_boundary(*offset))
        .unwrap_or(0);
    let Some((range, markup)) = image_at(text, middle..middle) else {
        panic!("{text:?} should be read as an image");
    };
    assert_eq!(
        range,
        0..text.len(),
        "the image should span all of {text:?}"
    );
    markup
}

#[test]
fn writes_markdown_when_nothing_needs_html() {
    assert_eq!(
        image("images/photo.png", "写真").render(""),
        inline("![写真](images/photo.png)")
    );

    let mut titled = image("images/photo.png", "写真");
    titled.title = Some("説明".into());
    assert_eq!(
        titled.render(""),
        inline("![写真](images/photo.png \"説明\")")
    );
}

#[test]
fn writes_an_img_tag_for_a_size() {
    let mut sized = image("images/photo.png", "写真");
    sized.width = Some("300".into());
    assert_eq!(
        sized.render(""),
        inline("<img src=\"images/photo.png\" alt=\"写真\" width=\"300\">")
    );

    sized.height = Some("50%".into());
    assert_eq!(
        sized.render(""),
        inline("<img src=\"images/photo.png\" alt=\"写真\" width=\"300\" height=\"50%\">")
    );

    let mut height_only = image("images/photo.png", "写真");
    height_only.height = Some("120".into());
    assert_eq!(
        height_only.render(""),
        inline("<img src=\"images/photo.png\" alt=\"写真\" height=\"120\">")
    );
}

#[test]
fn writes_one_html_block_for_an_alignment() {
    let mut centered = image("images/photo.png", "写真");
    centered.alignment = Some(ImageAlignment::Center);
    assert_eq!(
        centered.render(""),
        block("<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"写真\">\n</div>")
    );

    // Inside a list item every line after the first keeps the item's indent,
    // or the block would fall out of the list.
    let mut right = image("images/photo.png", "写真");
    right.alignment = Some(ImageAlignment::Right);
    right.width = Some("300".into());
    assert_eq!(
        right.render("  "),
        block(
            "<div align=\"right\">\n    <img src=\"images/photo.png\" alt=\"写真\" width=\"300\">\n  </div>"
        )
    );

    let mut left = image("a.png", "");
    left.alignment = Some(ImageAlignment::Left);
    assert!(
        left.render("").text.starts_with("<div align=\"left\">"),
        "{:?}",
        left.render("")
    );
    assert!(
        !left.render("").text.contains("\n\n"),
        "a blank line would split the HTML block"
    );
}

#[test]
fn escapes_what_each_form_cannot_hold() {
    let awkward = image("images/my photo (1)#2?.png", "a \"b\" & [c]");
    assert_eq!(
        awkward.render("").text,
        "![a \"b\" & \\[c\\]](images/my%20photo%20%281%29%232%3F.png)"
    );

    let mut sized = awkward;
    sized.width = Some("10".into());
    assert_eq!(
        sized.render("").text,
        "<img src=\"images/my%20photo%20(1)%232%3F.png\" alt=\"a &quot;b&quot; &amp; [c]\" width=\"10\">"
    );

    assert_eq!(image("100%.png", "x").render("").text, "![x](100%25.png)");
    // Japanese names and `../` are written as they are.
    assert_eq!(
        image("../画像/写真 1.png", "写真").render("").text,
        "![写真](../画像/写真%201.png)"
    );

    // A URL keeps its own escapes and query.
    let url = image("https://example.com/a%20b.png?x=1&y=2", "x");
    assert_eq!(
        url.render("").text,
        "![x](https://example.com/a%20b.png?x=1&y=2)"
    );
    let mut sized_url = url;
    sized_url.width = Some("10".into());
    assert!(
        sized_url
            .render("")
            .text
            .contains("src=\"https://example.com/a%20b.png?x=1&amp;y=2\""),
        "{:?}",
        sized_url.render("")
    );

    let mut titled = image("a.png", "x");
    titled.title = Some("say \"hi\" \\".into());
    assert_eq!(
        titled.render("").text,
        "![x](a.png \"say \\\"hi\\\" \\\\\")"
    );
}

#[test]
fn markdown_escapes_read_back_as_they_were_written() {
    for text in [
        "plain",
        "C:\\path\\to",
        "ends with \\",
        "[bracketed]",
        "a\\[b",
        "a\\\\b",
        "a\\*b",
        "日本語\\",
    ] {
        let escaped = escape_markdown(text, &['[', ']']);
        assert_eq!(
            unescape_markdown(&escaped, &['[', ']']),
            text,
            "{text:?} was written as {escaped:?}"
        );
    }
}

#[test]
fn reads_a_markdown_image_and_writes_it_back_unchanged() {
    for source in [
        "![写真](images/a%20b.png \"説明\")",
        "![a \\[1\\]](x.png)",
        "![](a.png)",
        "![x](../assets/logo.svg)",
    ] {
        assert_eq!(parse_whole(source).render("").text, source);
    }

    let angle = parse_whole("![a](<my file.png>)");
    assert_eq!(angle.destination, "my file.png");
    assert_eq!(angle.render("").text, "![a](my%20file.png)");

    let parentheses = parse_whole("![a](a(1).png 'it\\'s')");
    assert_eq!(parentheses.destination, "a(1).png");
    assert_eq!(parentheses.title.as_deref(), Some("it's"));

    let titled = parse_whole("![a](a.png (括弧の説明))");
    assert_eq!(titled.title.as_deref(), Some("括弧の説明"));

    // Escapes in a destination stand for the character they escape; what the
    // destination cannot hold is written back percent-encoded, never with a
    // backslash that would now mean something else.
    let escaped = parse_whole("![a](my\\_file\\(1\\).png)");
    assert_eq!(escaped.destination, "my_file(1).png");
    assert_eq!(escaped.render("").text, "![a](my_file%281%29.png)");
    let entity = parse_whole("![a](a&amp;b.png)");
    assert_eq!(entity.destination, "a&b.png");
}

#[test]
fn a_read_destination_is_written_back_as_the_document_spelled_it() {
    // GitHub's dark-mode picture and a raw-file query are not part of the file
    // name, and must not be encoded into it when only the size changes.
    let mut dark = parse_whole("![logo](images/logo.png#gh-dark-mode-only)");
    assert_eq!(dark.destination, "images/logo.png#gh-dark-mode-only");
    dark.width = Some("100".into());
    assert_eq!(
        dark.render("").text,
        "<img src=\"images/logo.png#gh-dark-mode-only\" alt=\"logo\" width=\"100\">"
    );
    let raw = parse_whole("![a](images/a.png?raw=true)");
    assert_eq!(raw.render("").text, "![a](images/a.png?raw=true)");

    // Escapes the author chose stay as they were.
    let mut encoded =
        parse_whole("<img src=\"images/%E5%86%99%E7%9C%9F.png\" alt=\"写真\" width=\"10\">");
    assert_eq!(encoded.destination, "images/写真.png");
    encoded.width = None;
    assert_eq!(
        encoded.render("").text,
        "![写真](images/%E5%86%99%E7%9C%9F.png)"
    );

    // A backslash that stood for itself in an attribute must not turn into a
    // Markdown escape; before a letter it already stands for itself there.
    let mut escaped = parse_whole("<img src=\"dir\\_a.png\" alt=\"a\" width=\"10\">");
    escaped.width = None;
    assert_eq!(escaped.render("").text, "![a](dir%5C_a.png)");
    let mut windows = parse_whole("<img src=\"C:\\pics\\a.png\" alt=\"a\" width=\"10\">");
    windows.width = None;
    assert_eq!(windows.render("").text, "![a](C:\\pics\\a.png)");

    // An ideographic space does not end a destination, as it does not for the
    // preview.
    assert_eq!(
        parse_whole("![写真](写真\u{3000}1.png)").destination,
        "写真\u{3000}1.png"
    );

    // A path typed anew is encoded from what the field holds, line breaks and
    // tabs included.
    let mut retyped = raw;
    retyped.written_destination = None;
    retyped.destination = "images/a#1\t.png".into();
    assert_eq!(retyped.render("").text, "![a](images/a%231%09.png)");
}

#[test]
fn reads_what_keeps_a_line_in_its_lists_and_quotes() {
    let item = line_prefix("- item");
    assert_eq!((item.length, item.continuation.as_str()), (2, "  "));
    assert_eq!(item.list_marker_column, Some(0));
    assert_eq!(line_prefix("  10. item").continuation, "      ");
    let quoted_item = line_prefix("> - item");
    assert_eq!(quoted_item.continuation, ">   ");
    assert_eq!(quoted_item.quote_depth, 1);
    assert_eq!(quoted_item.list_marker_column, Some(2));
    // A marker typed without its space still opens an empty item.
    assert_eq!(line_prefix("-").continuation, "  ");
    for content in ["**bold**", "---", "-1 degree", "text"] {
        assert_eq!(line_prefix(content).length, 0, "{content:?}");
    }

    assert!(is_blank_within_containers(">"));
    assert!(is_blank_within_containers("  "));
    assert!(is_blank_within_containers("- "));
    assert!(!is_blank_within_containers("> text"));
}

#[test]
fn finds_containers_inside_lists_and_quotes() {
    let listed = "- <div align=\"center\">\n    <img src=\"a.png\" alt=\"x\">\n  </div>";
    let Some((range, markup)) = image_at(listed, 30..30) else {
        panic!("the container in the list item should be found");
    };
    assert_eq!(range, 2..listed.len());
    assert_eq!(markup.alignment, Some(ImageAlignment::Center));
    assert_eq!(markup.render("  ").text, &listed[2..]);

    let quoted = "> <div align=\"right\">\n>   <img src=\"a.png\" alt=\"x\">\n> </div>";
    let Some((range, markup)) = image_at(quoted, 30..30) else {
        panic!("the container in the block quote should be found");
    };
    assert_eq!(range, 2..quoted.len());
    assert_eq!(markup.alignment, Some(ImageAlignment::Right));
    assert_eq!(markup.render("> ").text, &quoted[2..]);

    // A blank line of the quote, or a line that left it, ends the HTML block,
    // so what follows is not the container's.
    for split in [
        "> <div align=\"right\">\n>\n> <img src=\"a.png\">\n> </div>",
        "> <div align=\"right\">\n<img src=\"a.png\">\n> </div>",
    ] {
        let image = split.find("<img").unwrap_or_default() + 2;
        let Some((range, markup)) = image_at(split, image..image) else {
            panic!("the <img> in {split:?} should still be found");
        };
        assert!(markup.container.is_none(), "{split:?}");
        assert_eq!(&split[range], "<img src=\"a.png\">");
    }
}

#[test]
fn finds_the_image_the_cursor_or_selection_is_on() {
    let text = "text ![a](x.png) more";
    assert_eq!(image_at(text, 2..2), None);
    assert_eq!(image_at(text, 5..5).map(|(range, _)| range), Some(5..16));
    assert_eq!(image_at(text, 16..16).map(|(range, _)| range), Some(5..16));
    assert_eq!(image_at(text, 17..17), None);
    assert_eq!(image_at(text, 7..9).map(|(range, _)| range), Some(5..16));
    assert_eq!(
        image_at(text, 4..17).map(|(range, _)| range),
        Some(5..16),
        "whitespace around the image may be selected with it"
    );
    assert_eq!(
        image_at(text, 0..16),
        None,
        "a selection that holds more than the image is not the image"
    );

    assert_eq!(image_at("\\![a](x.png)", 5..5), None);
    assert_eq!(image_at("![a] (x.png)", 2..2), None);
}

#[test]
fn reads_html_images_and_their_containers() {
    let text = "before <img src=\"a.png\" alt=\"x\"> after";
    let Some((range, markup)) = image_at(text, 12..12) else {
        panic!("the inline <img> should be found");
    };
    assert_eq!(&text[range], "<img src=\"a.png\" alt=\"x\">");
    assert_eq!(markup.alt, "x");
    assert!(markup.container.is_none());

    let centered = "<div align=\"center\">\n  <img src=\"a.png\" alt=\"x\" width=\"300\">\n</div>";
    let markup = parse_whole(centered);
    assert_eq!(markup.alignment, Some(ImageAlignment::Center));
    assert_eq!(markup.width.as_deref(), Some("300"));
    assert_eq!(markup.render("").text, centered);

    // The container is found from its own lines as well as from the image.
    assert!(image_at(centered, 3..3).is_some_and(|(_, markup)| markup.container.is_some()));
    assert!(
        image_at(centered, centered.len() - 2..centered.len() - 2)
            .is_some_and(|(_, markup)| markup.container.is_some())
    );

    // A container written on one line is written back on one line.
    let mut one_line = parse_whole("<p align=\"right\"><img src=\"a.png\" alt=\"x\"></p>");
    assert_eq!(one_line.alignment, Some(ImageAlignment::Right));
    one_line.width = Some("40".into());
    assert_eq!(
        one_line.render("  ").text,
        "<p align=\"right\"><img src=\"a.png\" alt=\"x\" width=\"40\"></p>"
    );
    let mut new_container = parse_whole("<img src=\"a.png\" alt=\"x\">");
    new_container.alignment = Some(ImageAlignment::Right);
    assert_eq!(
        new_container.render("").text,
        "<div align=\"right\">\n  <img src=\"a.png\" alt=\"x\">\n</div>",
        "a container the image did not have starts out on lines of its own"
    );

    // A container that holds more than the image is not the image's.
    let crowded = "<div align=\"center\"><img src=\"a.png\"> text</div>";
    let Some((range, markup)) = image_at(crowded, 25..25) else {
        panic!("the <img> itself should still be found");
    };
    assert_eq!(&crowded[range], "<img src=\"a.png\">");
    assert!(markup.container.is_none());
}

#[test]
fn keeps_the_attributes_it_has_no_field_for() {
    let source = "<img class=\"hero\" src=\"a%20b.png\" loading=lazy alt=\"写真\" style=\"border: 1px solid; height: 120px\">";
    let markup = parse_whole(source);
    assert_eq!(markup.destination, "a b.png");
    assert_eq!(markup.height.as_deref(), Some("120"));
    assert_eq!(markup.style.as_deref(), Some("border: 1px solid"));
    assert_eq!(
        markup.render("").text,
        "<img class=\"hero\" src=\"a%20b.png\" loading=lazy alt=\"写真\" style=\"border: 1px solid\" height=\"120\">"
    );

    let mut resized = markup;
    resized.width = Some("200".into());
    resized.height = None;
    assert_eq!(
        resized.render("").text,
        "<img class=\"hero\" src=\"a%20b.png\" loading=lazy alt=\"写真\" style=\"border: 1px solid\" width=\"200\">"
    );

    let self_closing = "<img src=\"a.png\" alt=\"x\" width=\"10\" />";
    assert_eq!(parse_whole(self_closing).render("").text, self_closing);

    let entities =
        parse_whole("<img src=\"a&amp;b.png\" alt=\"&quot;q&quot; &#x41;\" width=\"1\">");
    assert_eq!(entities.destination, "a&b.png");
    assert_eq!(entities.alt, "\"q\" A");
    assert_eq!(
        entities.render("").text,
        "<img src=\"a&amp;b.png\" alt=\"&quot;q&quot; A\" width=\"1\">"
    );
    // With nothing Markdown cannot spell, the same image goes back to Markdown.
    let mut plain = entities;
    plain.width = None;
    assert_eq!(plain.render("").text, "![\"q\" A](a&b.png)");

    // A width the field cannot hold stays as it was until one is typed, and
    // an `alt` the tag never had is not added.
    let auto = parse_whole("<img src=\"a.png\" width=\"auto\">");
    assert_eq!(auto.width, None);
    assert_eq!(auto.render("").text, "<img src=\"a.png\" width=\"auto\">");
    let mut typed = auto;
    typed.width = Some("300".into());
    assert_eq!(typed.render("").text, "<img src=\"a.png\" width=\"300\">");
    typed.alt = "説明".into();
    assert_eq!(
        typed.render("").text,
        "<img src=\"a.png\" alt=\"説明\" width=\"300\">",
        "an alt text the user typed in is added"
    );
    // An empty `alt` that was there, which marks decoration, stays.
    assert_eq!(
        parse_whole("<img src=\"a.png\" alt=\"\" width=\"1\">")
            .render("")
            .text,
        "<img src=\"a.png\" alt=\"\" width=\"1\">"
    );
    // So does the empty one a Markdown image turned into HTML carries.
    let mut from_markdown = parse_whole("![](a.png)");
    from_markdown.width = Some("5".into());
    assert_eq!(
        from_markdown.render("").text,
        "<img src=\"a.png\" alt=\"\" width=\"5\">"
    );
}

#[test]
fn taking_the_layout_away_takes_the_html_away() {
    let mut markup =
        parse_whole("<p align=\"center\"><img src=\"a.png\" alt=\"x\" width=\"300\"></p>");
    markup.alignment = None;
    markup.width = None;
    assert_eq!(markup.render(""), inline("![x](a.png)"));

    // A title moves between the two spellings instead of being dropped.
    let mut titled = parse_whole("![x](a.png \"t\")");
    titled.width = Some("300".into());
    assert_eq!(
        titled.render("").text,
        "<img src=\"a.png\" alt=\"x\" width=\"300\" title=\"t\">"
    );
    let back = parse_whole("<img src=\"a.png\" alt=\"x\" title=\"t\">");
    assert_eq!(back.render("").text, "![x](a.png \"t\")");

    // A container with attributes of its own survives losing its alignment.
    let mut classed = parse_whole(
        "<div align=\"center\" class=\"hero\">\n  <img src=\"a.png\" alt=\"x\">\n</div>",
    );
    classed.alignment = None;
    assert_eq!(
        classed.render(""),
        block("<div class=\"hero\">\n  <img src=\"a.png\" alt=\"x\">\n</div>")
    );
    classed.alignment = Some(ImageAlignment::Right);
    assert_eq!(
        classed.render("").text,
        "<div align=\"right\" class=\"hero\">\n  <img src=\"a.png\" alt=\"x\">\n</div>"
    );

    // An alignment the form cannot show stays until one is picked.
    let mut justified =
        parse_whole("<div align=\"justify\">\n<img src=\"a.png\" alt=\"x\">\n</div>");
    assert_eq!(justified.alignment, None);
    assert_eq!(
        justified.render("").text,
        "<div align=\"justify\">\n  <img src=\"a.png\" alt=\"x\">\n</div>"
    );
    justified.alignment = Some(ImageAlignment::Center);
    assert_eq!(
        justified.render("").text,
        "<div align=\"center\">\n  <img src=\"a.png\" alt=\"x\">\n</div>"
    );
}

#[test]
fn reads_widths_and_heights() {
    assert_eq!(parse_length(""), LengthInput::Empty);
    assert_eq!(parse_length("   "), LengthInput::Empty);
    assert_eq!(parse_length("300"), LengthInput::Valid("300".into()));
    assert_eq!(parse_length(" 300px "), LengthInput::Valid("300".into()));
    assert_eq!(parse_length("50%"), LengthInput::Valid("50%".into()));
    assert_eq!(parse_length("50 %"), LengthInput::Valid("50%".into()));
    assert_eq!(parse_length("12.5"), LengthInput::Valid("12.5".into()));
    assert_eq!(parse_length("３００"), LengthInput::Valid("300".into()));
    assert_eq!(parse_length("５０％"), LengthInput::Valid("50%".into()));
    for invalid in ["abc", "30em", ".5", "5.", "1.2.3", "-3", "%", "px"] {
        assert_eq!(parse_length(invalid), LengthInput::Invalid, "{invalid:?}");
    }
}

#[test]
fn works_out_relative_paths() {
    let windows = PathStyle::Windows;
    assert_eq!(
        relative_path(r"C:\proj\docs", r"C:\proj\docs\images\a.png", windows).as_deref(),
        Some("images/a.png")
    );
    assert_eq!(
        relative_path(r"C:\proj\docs", r"c:\PROJ\assets\b.png", windows).as_deref(),
        Some("../assets/b.png"),
        "Windows paths compare without case"
    );
    assert_eq!(
        relative_path("C:/proj/docs", r"C:\proj\c.png", windows).as_deref(),
        Some("../c.png")
    );
    assert_eq!(relative_path(r"C:\proj\docs", r"D:\x.png", windows), None);
    assert_eq!(
        relative_path(r"\\server\share\docs", r"\\server\share\img\a.png", windows).as_deref(),
        Some("../img/a.png")
    );
    assert_eq!(
        relative_path(r"\\server\share\docs", r"\\other\share\img\a.png", windows),
        None
    );
    assert_eq!(relative_path(r"C:\proj\docs", r"C:\proj", windows), None);

    let unix = PathStyle::Unix;
    assert_eq!(
        relative_path("/home/u/docs", "/home/u/docs/a.png", unix).as_deref(),
        Some("a.png")
    );
    assert_eq!(
        relative_path("/home/u/docs", "/tmp/x.png", unix).as_deref(),
        Some("../../../tmp/x.png")
    );
    assert_eq!(
        relative_path("/home/u/docs", "/Home/u/docs/a.png", unix).as_deref(),
        Some("../../../Home/u/docs/a.png"),
        "Unix paths compare with case"
    );

    assert_eq!(
        folder_distance(r"C:\p\docs", r"C:\p\docs\a.png", windows),
        Some(0)
    );
    assert_eq!(
        folder_distance(r"C:\p\docs", r"C:\p\docs\images\a.png", windows),
        Some(1)
    );
    assert_eq!(
        folder_distance(r"C:\p\docs", r"C:\p\assets\a.png", windows),
        Some(2)
    );
    assert_eq!(folder_distance(r"C:\p\docs", r"D:\a.png", windows), None);
}

fn windows_document() -> DocumentLocation {
    DocumentLocation {
        directory: Some(r"C:\proj\docs".into()),
        root: Some(r"C:\proj".into()),
        path_style: PathStyle::Windows,
        is_local: true,
    }
}

#[test]
fn resolves_what_the_path_field_holds() {
    let location = windows_document();
    let file = |input: &str| match resolve_source(input, &location) {
        ResolvedSource::File(file) => file,
        other => panic!("{input:?} should be a file, got {other:?}"),
    };

    assert_eq!(
        file(r"C:\proj\docs\images\a.png"),
        ResolvedFile {
            absolute: Some(r"C:\proj\docs\images\a.png".into()),
            relative: Some("images/a.png".into()),
            inside_root: true,
        }
    );
    let outside = file(r"C:\outside\b.png");
    assert_eq!(outside.relative.as_deref(), Some("../../outside/b.png"));
    assert!(!outside.inside_root);
    let other_drive = file(r"D:\pics\c.png");
    assert_eq!(other_drive.relative, None);
    assert_eq!(other_drive.destination().as_deref(), Some("D:/pics/c.png"));

    let typed = file(r".\images\a.png");
    assert_eq!(typed.relative.as_deref(), Some("images/a.png"));
    assert_eq!(
        typed.absolute.as_deref(),
        Some(r"C:\proj\docs\images\a.png")
    );
    assert!(typed.inside_root);
    assert!(!file("../../elsewhere/a.png").inside_root);

    assert_eq!(
        file(r#""C:\proj\docs\a b.png""#).relative.as_deref(),
        Some("a b.png"),
        "the quotes of Windows' Copy as path are dropped"
    );
    assert_eq!(
        resolve_source("https://example.com/a.png", &location),
        ResolvedSource::Url("https://example.com/a.png".into())
    );
    assert_eq!(resolve_source("  ", &location), ResolvedSource::Empty);
    assert_eq!(
        file("/images/a.png"),
        ResolvedFile {
            absolute: None,
            relative: Some("/images/a.png".into()),
            inside_root: false,
        },
        "a rooted path with no drive is kept for the preview to read from the project root"
    );

    let unsaved = DocumentLocation {
        directory: None,
        root: None,
        ..windows_document()
    };
    assert_eq!(
        resolve_source("images/a.png", &unsaved),
        ResolvedSource::File(ResolvedFile {
            absolute: None,
            relative: Some("images/a.png".into()),
            inside_root: false,
        })
    );
    let ResolvedSource::File(absolute) = resolve_source(r"C:\x\a.png", &unsaved) else {
        panic!("an absolute path is a file");
    };
    assert_eq!(absolute.destination().as_deref(), Some("C:/x/a.png"));

    let remote = DocumentLocation {
        directory: Some("/home/u/docs".into()),
        root: Some("/home/u".into()),
        path_style: PathStyle::Unix,
        is_local: false,
    };
    let ResolvedSource::File(remote_file) = resolve_source("/home/u/img/a.png", &remote) else {
        panic!("a remote path is a file");
    };
    assert_eq!(remote_file.relative.as_deref(), Some("../img/a.png"));
    assert!(remote_file.inside_root);
}

#[test]
fn starts_the_alt_text_from_the_file_name() {
    assert_eq!(default_alt("images/写真.png"), "写真");
    assert_eq!(default_alt(r"C:\pics\my.photo.jpeg"), "my.photo");
    assert_eq!(default_alt("https://example.com/a/b.png?x=1"), "b");
    assert_eq!(default_alt("noextension"), "noextension");
    assert_eq!(default_alt(""), "");
}

#[test]
fn recognises_image_files() {
    assert!(has_image_extension("a/b/photo.PNG"));
    assert!(has_image_extension(r"C:\x\y.avif"));
    assert!(!has_image_extension("notes.md"));
    assert!(!has_image_extension("images/.png"));
    assert!(!has_image_extension("png"));
}

fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
    cx.update(|cx| {
        let state = workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        state
    })
}

fn register_markdown_language(project: &Entity<Project>, cx: &mut TestAppContext) {
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(Arc::new(language::Language::new(
        language::LanguageConfig {
            name: "Markdown".into(),
            matcher: Arc::new(language::LanguageMatcher {
                path_suffixes: vec!["md".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        },
        None,
    )));
}

struct Opened {
    workspace: Entity<Workspace>,
    editor: Entity<Editor>,
    fs: Arc<FakeFs>,
    _app_state: Arc<workspace::AppState>,
}

/// A project with `docs/readme.md` holding `marked` (cursor and selection
/// marked), an image beside it, one elsewhere in the project and one outside
/// it, with the Markdown editor focused.
async fn open_markdown<'a>(
    marked: &str,
    cx: &'a mut TestAppContext,
) -> (Opened, &'a mut VisualTestContext) {
    let app_state = init_test(cx);
    let (text, selections) = marked_text_ranges(marked, true);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({
            "docs": {
                "readme.md": text,
                "images": {"photo.png": "not really a png"},
            },
            "assets": {"logo.svg": "<svg/>"},
        }),
    )
    .await;
    fs.insert_tree(path!("/outside"), json!({"pic.png": "outside"}))
        .await;
    fs.insert_file(path!("/project/docs/images/real.png"), PNG_3_BY_2.to_vec())
        .await;
    open_in_project(
        app_state,
        fs,
        path!("/project"),
        rel_path("docs/readme.md"),
        selections,
        cx,
    )
    .await
}

/// Opens `document` of a project on `root`, which may be the document itself,
/// with `selections` made and the editor focused.
async fn open_in_project<'a>(
    app_state: Arc<workspace::AppState>,
    fs: Arc<FakeFs>,
    root: &str,
    document: &RelPath,
    selections: Vec<Range<usize>>,
    cx: &'a mut TestAppContext,
) -> (Opened, &'a mut VisualTestContext) {
    let project = Project::test(fs.clone(), [root.as_ref()], cx).await;
    register_markdown_language(&project, cx);
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id())
            .expect("the project has a worktree")
    });
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, document), None, true, window, cx)
        })
        .await
        .unwrap_or_else(|error| panic!("the document should open: {error:#}"));
    let editor = item
        .downcast::<Editor>()
        .expect("the document opens in an editor");
    cx.run_until_parked();

    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections_collection| {
            selections_collection.select_ranges(
                selections
                    .iter()
                    .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end)),
            );
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.run_until_parked();

    (
        Opened {
            workspace,
            editor,
            fs,
            _app_state: app_state,
        },
        cx,
    )
}

/// Dispatches `markdown::InsertImage` from the editor and returns the dialog
/// it opened.
fn open_dialog(opened: &Opened, cx: &mut VisualTestContext) -> Entity<ImageDialog> {
    cx.dispatch_action(InsertImage);
    cx.run_until_parked();
    opened
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ImageDialog>(cx)
        })
        .expect("the image dialog should be open")
}

fn set_field(
    dialog: &Entity<ImageDialog>,
    field: impl Fn(&ImageDialog) -> Entity<Editor>,
    text: &str,
    cx: &mut VisualTestContext,
) {
    let field = dialog.read_with(cx, |dialog, _| field(dialog));
    cx.update(|window, cx| field.update(cx, |field, cx| field.set_text(text, window, cx)));
    cx.run_until_parked();
}

fn field_text(
    dialog: &Entity<ImageDialog>,
    field: impl Fn(&ImageDialog) -> Entity<Editor>,
    cx: &mut VisualTestContext,
) -> String {
    let field = dialog.read_with(cx, |dialog, _| field(dialog));
    field.read_with(cx, |field, cx| field.text(cx))
}

fn buffer_text(opened: &Opened, cx: &mut VisualTestContext) -> String {
    opened.editor.read_with(cx, |editor, cx| editor.text(cx))
}

fn dialog_is_open(opened: &Opened, cx: &mut VisualTestContext) -> bool {
    opened
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ImageDialog>(cx)
        })
        .is_some()
}

#[gpui::test]
async fn inserts_a_picked_image_as_markdown(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("Intro ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    let listed = dialog.read_with(cx, |dialog, _| {
        dialog
            .matches
            .iter()
            .filter_map(|found| dialog.candidates.get(found.candidate_id))
            .map(|candidate| candidate.display.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        listed,
        vec![
            "images/photo.png".to_string(),
            "images/real.png".to_string(),
            "../assets/logo.svg".to_string(),
        ],
        "the list holds the project's pictures, nearest to the document first"
    );

    set_field(&dialog, |dialog| dialog.path_field.clone(), "photo", cx);
    let filtered = dialog.read_with(cx, |dialog, _| {
        dialog
            .matches
            .first()
            .and_then(|found| dialog.candidates.get(found.candidate_id))
            .map(|candidate| candidate.display.clone())
    });
    assert_eq!(filtered.as_deref(), Some("images/photo.png"));

    dialog.update_in(cx, |dialog, window, cx| {
        dialog.pick_candidate(0, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        field_text(&dialog, |dialog| dialog.alt_field.clone(), cx),
        "photo",
        "the alt text starts out as the file name"
    );

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "Intro ![photo](images/photo.png)\n"
    );
    assert!(!dialog_is_open(&opened, cx));
}

#[gpui::test]
async fn arrow_keys_and_enter_pick_from_the_list(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    cx.dispatch_action(MoveDown);
    cx.dispatch_action(MoveDown);
    cx.run_until_parked();
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.highlighted),
        Some(1)
    );

    // The first Enter takes the highlighted file; the second writes it.
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        field_text(&dialog, |dialog| dialog.path_field.clone(), cx),
        "images/real.png"
    );
    assert_eq!(buffer_text(&opened, cx), "\n");

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "![real](images/real.png)\n");
}

#[gpui::test]
async fn reports_the_size_of_the_picked_image(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/real.png",
        cx,
    );
    let details = dialog.read_with(cx, |dialog, _| dialog.details.clone());
    let SourceDetails::Found { dimensions, .. } = details else {
        panic!("the file exists, got {details:?}");
    };
    assert_eq!(dimensions, Some((3, 2)));

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/missing.png",
        cx,
    );
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.details.clone()),
        SourceDetails::Missing
    );
    let warnings = cx.update(|_window, cx| dialog.read(cx).warnings(cx));
    assert_eq!(
        warnings,
        vec![SharedString::from("No file was found at this path.")]
    );

    // An SVG's size is read from its `<svg>` tag.
    opened
        .fs
        .insert_file(
            path!("/project/assets/logo.svg"),
            b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"48\" viewBox=\"0 0 24 12\"/>".to_vec(),
        )
        .await;
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "../assets/logo.svg",
        cx,
    );
    let details = dialog.read_with(cx, |dialog, _| dialog.details.clone());
    let SourceDetails::Found { dimensions, .. } = details else {
        panic!("the SVG exists, got {details:?}");
    };
    assert_eq!(dimensions, Some((48, 24)));
}

#[test]
fn reads_the_size_of_an_svg_from_its_tag() {
    assert_eq!(
        svg_dimensions(b"<svg width=\"120\" height=\"80\"></svg>"),
        Some((120, 80))
    );
    assert_eq!(
        svg_dimensions(
            b"<?xml version=\"1.0\"?>\n<!-- <svg width=\"1\" height=\"1\"> -->\n<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 300 150\">"
        ),
        Some((300, 150)),
        "the tag inside a comment before it is not the drawing's"
    );
    assert_eq!(
        svg_dimensions(b"<svg width=\"600\" viewBox=\"0,0,300,150\">"),
        Some((600, 300))
    );
    assert_eq!(
        svg_dimensions(b"<svg height=\"50px\" viewBox=\"0 0 300 150\">"),
        Some((100, 50))
    );
    assert_eq!(
        svg_dimensions(b"<svg width=\"100%\" height=\"100%\" viewBox=\"0 0 30 20\">"),
        Some((30, 20)),
        "a percentage says nothing about pixels, so the view box is used"
    );
    assert_eq!(
        svg_dimensions(b"<svg width=\"12.6px\" height=\"7.4\">"),
        Some((13, 7))
    );
    assert_eq!(svg_dimensions(b"<svg width=\"10cm\" height=\"5cm\">"), None);
    assert_eq!(svg_dimensions(b"<svg/>"), None);
    assert_eq!(svg_dimensions(b"not a picture"), None);
}

#[gpui::test]
async fn an_alignment_writes_a_block_set_apart_by_blank_lines(cx: &mut TestAppContext) {
    // An image already in the middle of a line is split out where it stands.
    let (opened, cx) =
        open_markdown("Before\nIntro ![写真](images/phˇoto.png) text\nAfter\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(&dialog, |dialog| dialog.width_field.clone(), "300", cx);
    dialog.update(cx, |dialog, cx| {
        dialog.alignment = Some(ImageAlignment::Center);
        cx.notify();
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert_eq!(
        buffer_text(&opened, cx),
        "Before\nIntro \n\n<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"写真\" width=\"300\">\n</div>\n\n text\nAfter\n"
    );
}

#[gpui::test]
async fn a_block_on_its_own_line_gets_only_the_blank_lines_it_lacks(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("Before\nˇ\nAfter\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    dialog.update(cx, |dialog, cx| {
        dialog.alignment = Some(ImageAlignment::Right);
        cx.notify();
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert_eq!(
        buffer_text(&opened, cx),
        "Before\n\n<div align=\"right\">\n  <img src=\"images/photo.png\" alt=\"photo\">\n</div>\n\nAfter\n"
    );
}

fn set_alignment(
    dialog: &Entity<ImageDialog>,
    alignment: Option<ImageAlignment>,
    cx: &mut VisualTestContext,
) {
    dialog.update(cx, |dialog, cx| {
        dialog.alignment = alignment;
        cx.notify();
    });
}

/// Puts `marked` into the document with its cursor and selection, so one
/// test can try several documents.
fn reset_document(opened: &Opened, marked: &str, cx: &mut VisualTestContext) {
    let (text, selections) = marked_text_ranges(marked, true);
    opened.editor.update_in(cx, |editor, window, cx| {
        editor.set_text(text, window, cx);
        editor.change_selections(Default::default(), window, cx, |selections_collection| {
            selections_collection.select_ranges(
                selections
                    .iter()
                    .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end)),
            );
        });
    });
    cx.run_until_parked();
}

/// Opens the dialog on `marked`, picks the photo when nothing is there yet,
/// sets `alignment` and confirms, returning the document and what the dialog
/// said if it stayed open.
fn write_with_alignment(
    opened: &Opened,
    marked: &str,
    alignment: Option<ImageAlignment>,
    cx: &mut VisualTestContext,
) -> (String, Option<SharedString>) {
    reset_document(opened, marked, cx);
    let dialog = open_dialog(opened, cx);
    if !dialog.read_with(cx, |dialog, _| dialog.is_editing()) {
        set_field(
            &dialog,
            |dialog| dialog.path_field.clone(),
            "images/photo.png",
            cx,
        );
    }
    set_alignment(&dialog, alignment, cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    let message = dialog_is_open(opened, cx)
        .then(|| dialog.read_with(cx, |dialog, _| dialog.message.clone()))
        .flatten();
    if dialog_is_open(opened, cx) {
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
    }
    (buffer_text(opened, cx), message)
}

const CENTERED_PHOTO: &str =
    "<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"photo\">\n</div>";

#[gpui::test]
async fn an_aligned_image_typed_mid_line_goes_after_the_paragraph(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let center = Some(ImageAlignment::Center);

    let (text, _) = write_with_alignment(&opened, "Intro ˇtext\nmore lines\n\nNext\n", center, cx);
    assert_eq!(
        text,
        format!("Intro text\nmore lines\n\n{CENTERED_PHOTO}\n\nNext\n")
    );

    // A lazy line still belongs to the list item, whose indent the block
    // keeps; in a tight list no blank line goes before it, which would
    // loosen the list.
    let (text, _) = write_with_alignment(&opened, "- item\nlazy ˇtext\n- next\n", center, cx);
    assert_eq!(
        text,
        "- item\nlazy text\n  <div align=\"center\">\n    <img src=\"images/photo.png\" alt=\"photo\">\n  </div>\n- next\n"
    );

    // The line the block follows is read by itself, even when it is shorter
    // than the marker of the item it belongs to.
    let (text, _) = write_with_alignment(&opened, "10. item\nab ˇc\n", center, cx);
    assert_eq!(
        text,
        "10. item\nab c\n    <div align=\"center\">\n      <img src=\"images/photo.png\" alt=\"photo\">\n    </div>\n"
    );

    // A loose list has its blank lines already, and gets them here too.
    let (text, _) = write_with_alignment(&opened, "- item ˇtext\n\n- next\n", center, cx);
    assert_eq!(
        text,
        "- item text\n\n  <div align=\"center\">\n    <img src=\"images/photo.png\" alt=\"photo\">\n  </div>\n\n- next\n"
    );

    let (text, _) = write_with_alignment(&opened, "# 見出ˇし\nbody\n", center, cx);
    assert_eq!(text, format!("# 見出し\n\n{CENTERED_PHOTO}\n\nbody\n"));

    // The underline of a setext heading is part of it.
    let (text, _) = write_with_alignment(&opened, "タイˇトル\n===\nbody\n", center, cx);
    assert_eq!(text, format!("タイトル\n===\n\n{CENTERED_PHOTO}\n\nbody\n"));

    let (text, _) = write_with_alignment(&opened, "> Intro ˇmore\n> end\n", center, cx);
    assert_eq!(
        text,
        "> Intro more\n> end\n>\n> <div align=\"center\">\n>   <img src=\"images/photo.png\" alt=\"photo\">\n> </div>\n"
    );

    // Inside link text there is nothing to refuse once the line is not split.
    let (text, message) = write_with_alignment(
        &opened,
        "[ˇfoo bar](https://example.com) tail\n",
        center,
        cx,
    );
    assert_eq!(message, None);
    assert_eq!(
        text,
        format!("[foo bar](https://example.com) tail\n\n{CENTERED_PHOTO}\n")
    );

    // At the start or the end of a line, and without an alignment, the image
    // goes where the cursor is.
    let (text, _) = write_with_alignment(&opened, "Intro ˇtext\n", None, cx);
    assert_eq!(text, "Intro ![photo](images/photo.png)text\n");
    let (text, _) = write_with_alignment(&opened, "Intro textˇ\n\nNext\n", center, cx);
    assert_eq!(text, format!("Intro text\n\n{CENTERED_PHOTO}\n\nNext\n"));
}

#[gpui::test]
async fn a_selection_stays_when_its_aligned_image_goes_after_the_paragraph(
    cx: &mut TestAppContext,
) {
    let (opened, cx) = open_markdown("See «the logoˇ» here\nmore\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.placement_note()),
        None
    );
    set_alignment(&dialog, Some(ImageAlignment::Center), cx);
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.placement_note()),
        Some(
            "An aligned image goes after the paragraph the cursor is in. The selected text stays as it is."
        )
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "See the logo here\nmore\n\n<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"the logo\">\n</div>\n"
    );
}

#[test]
fn tells_html_blocks_from_html_in_paragraphs() {
    // A comment ends on its own line, and what follows is Markdown again.
    assert_eq!(html_blocks("<!-- note -->\n![a](b.png)"), vec![0..13]);
    // A lone tag cannot interrupt a paragraph, but a block-level one can.
    assert_eq!(
        html_blocks("text\n<img src=\"a.png\">"),
        Vec::<Range<usize>>::new()
    );
    let interrupted = "text\n<div>\n<img src=\"a.png\">\n</div>";
    assert_eq!(html_blocks(interrupted), vec![5..interrupted.len()]);

    // An `<img>` that is a block by itself is not inside one.
    let lone = "<img src=\"a.png\" width=\"1\">";
    assert_eq!(html_blocks(lone), vec![0..lone.len()]);
    assert!(!is_inside_html(&html_blocks(lone), lone, &(0..lone.len())));
    let quoted = "> <p>\n> <img src=\"a.png\"> 説明\n> </p>";
    let image = quoted.find("<img").unwrap_or_default();
    assert!(is_inside_html(
        &html_blocks(quoted),
        quoted,
        &(image..image + "<img src=\"a.png\">".len())
    ));

    assert!(leaves_an_element_open("<a href=\"x\">"));
    assert!(leaves_an_element_open("<span>text <b>x</b> "));
    assert!(!leaves_an_element_open("<b>注意</b> "));
    assert!(!leaves_an_element_open("<img src=\"a\"> <br>"));
}

#[gpui::test]
async fn an_alignment_is_refused_inside_html(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let center = Some(ImageAlignment::Center);
    let refusal =
        SharedString::from("An image inside HTML cannot be aligned. Set Alignment to \"Not Set\".");

    for marked in [
        "<a href=\"https://example.com\"><img src=\"images/phˇoto.png\" alt=\"a\"></a>\n",
        "<table>\n<tr><td><img src=\"images/phˇoto.png\" alt=\"a\"></td></tr>\n</table>\n",
        "<p><img src=\"images/phˇoto.png\" alt=\"a\"> 説明</p>\n",
        "<div>\ntext ˇ\n</div>\n",
    ] {
        let (text, message) = write_with_alignment(&opened, marked, center, cx);
        assert_eq!(message, Some(refusal.clone()), "{marked:?}");
        assert_eq!(text, marked_text_ranges(marked, false).0, "{marked:?}");
    }

    // A size alone is written in place, and never as Markdown, which an HTML
    // block would show as it is.
    reset_document(
        &opened,
        "<div>\n<img src=\"images/phˇoto.png\" alt=\"a\" width=\"10\">\n説明\n</div>\n",
        cx,
    );
    let dialog = open_dialog(&opened, cx);
    set_field(&dialog, |dialog| dialog.width_field.clone(), "", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "<div>\n<img src=\"images/photo.png\" alt=\"a\">\n説明\n</div>\n"
    );
    let (text, _) = write_with_alignment(&opened, "<div>\ntext ˇ\n</div>\n", None, cx);
    assert_eq!(
        text,
        "<div>\ntext <img src=\"images/photo.png\" alt=\"photo\">\n</div>\n"
    );

    // A container inside an HTML block is rewritten in place, without the
    // blank lines that would end the outer block.
    let (text, _) = write_with_alignment(
        &opened,
        "<div>\n<div align=\"center\">\n  <img src=\"images/phˇoto.png\" alt=\"a\">\n</div>\n</div>\n",
        Some(ImageAlignment::Right),
        cx,
    );
    assert_eq!(
        text,
        "<div>\n<div align=\"right\">\n  <img src=\"images/photo.png\" alt=\"a\">\n</div>\n</div>\n"
    );

    // An `<img>` that is a block of its own, or follows a closed element, can
    // take an alignment.
    let (text, message) = write_with_alignment(
        &opened,
        "<img src=\"images/phˇoto.png\" alt=\"photo\" width=\"10\">\n",
        center,
        cx,
    );
    assert_eq!(message, None);
    assert_eq!(
        text,
        "<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"photo\" width=\"10\">\n</div>\n"
    );
    let (text, message) = write_with_alignment(
        &opened,
        "<b>注意</b> ![photo](images/phˇoto.png)\n",
        center,
        cx,
    );
    assert_eq!(message, None);
    assert_eq!(text, format!("<b>注意</b> \n\n{CENTERED_PHOTO}\n"));
}

#[gpui::test]
async fn an_alignment_inside_a_list_item_keeps_the_list(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("- first\n- ˇ\n- next\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    set_alignment(&dialog, Some(ImageAlignment::Center), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    let written = "- first\n- <div align=\"center\">\n    <img src=\"images/photo.png\" alt=\"photo\">\n  </div>\n- next\n";
    assert_eq!(buffer_text(&opened, cx), written);

    // The dialog reads back what it wrote there, and writes it again as it was.
    let dialog = open_dialog(&opened, cx);
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.alignment),
        Some(ImageAlignment::Center)
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), written);
}

#[gpui::test]
async fn an_alignment_inside_a_block_quote_stays_in_the_quote(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("> Intro ![a](images/phˇoto.png) more\n> end\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_alignment(&dialog, Some(ImageAlignment::Right), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert_eq!(
        buffer_text(&opened, cx),
        "> Intro \n>\n> <div align=\"right\">\n>   <img src=\"images/photo.png\" alt=\"a\">\n> </div>\n>\n>  more\n> end\n"
    );
}

#[gpui::test]
async fn an_alignment_is_refused_where_a_block_would_break_the_line(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("| 画像 |\n| --- |\n| ˇ |\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    set_alignment(&dialog, Some(ImageAlignment::Center), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "| 画像 |\n| --- |\n|  |\n");
    assert_eq!(
        dialog
            .read_with(cx, |dialog, _| dialog.message.clone())
            .as_deref(),
        Some("An image inside a table cannot be aligned. Set Alignment to \"Not Set\".")
    );

    // Without the alignment the image fits in the cell.
    set_alignment(&dialog, None, cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "| 画像 |\n| --- |\n| ![photo](images/photo.png) |\n"
    );
}

#[gpui::test]
async fn an_alignment_is_refused_for_a_linked_image(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("[![a](images/phˇoto.png)](https://example.com)\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    assert!(dialog.read_with(cx, |dialog, _| dialog.is_editing()));
    set_alignment(&dialog, Some(ImageAlignment::Center), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "[![a](images/photo.png)](https://example.com)\n"
    );
    assert_eq!(
        dialog
            .read_with(cx, |dialog, _| dialog.message.clone())
            .as_deref(),
        Some("An image inside a link cannot be aligned. Set Alignment to \"Not Set\".")
    );

    // A size alone is written in place, inside the link.
    set_alignment(&dialog, None, cx);
    set_field(&dialog, |dialog| dialog.width_field.clone(), "20", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "[<img src=\"images/photo.png\" alt=\"a\" width=\"20\">](https://example.com)\n"
    );
}

#[gpui::test]
async fn a_path_left_alone_keeps_its_fragment_and_a_new_one_is_encoded(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("![logo](images/logo.png#gh-dark-mode-onlyˇ)\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(&dialog, |dialog| dialog.width_field.clone(), "100", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "<img src=\"images/logo.png#gh-dark-mode-only\" alt=\"logo\" width=\"100\">\n"
    );

    opened.editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(5)..MultiBufferOffset(5)]);
        });
    });
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/new#1.png",
        cx,
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "<img src=\"images/new%231.png\" alt=\"logo\" width=\"100\">\n"
    );
}

#[gpui::test]
async fn line_breaks_pasted_into_the_alt_text_become_spaces(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    set_field(
        &dialog,
        |dialog| dialog.alt_field.clone(),
        "一行目\n\n二行目",
        cx,
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "![一行目 二行目](images/photo.png)\n"
    );
}

#[gpui::test]
async fn editing_a_centered_image_reads_it_back_and_replaces_it(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown(
        "Text\n\n<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"写ˇ真\" width=\"300\" loading=\"lazy\">\n</div>\n\nMore\n",
        cx,
    )
    .await;
    let dialog = open_dialog(&opened, cx);

    assert!(dialog.read_with(cx, |dialog, _| dialog.is_editing()));
    assert_eq!(
        field_text(&dialog, |dialog| dialog.path_field.clone(), cx),
        "images/photo.png"
    );
    assert_eq!(
        field_text(&dialog, |dialog| dialog.alt_field.clone(), cx),
        "写真"
    );
    assert_eq!(
        field_text(&dialog, |dialog| dialog.width_field.clone(), cx),
        "300"
    );
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.alignment),
        Some(ImageAlignment::Center)
    );

    set_field(&dialog, |dialog| dialog.width_field.clone(), "50%", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "Text\n\n<div align=\"center\">\n  <img src=\"images/photo.png\" alt=\"写真\" width=\"50%\" loading=\"lazy\">\n</div>\n\nMore\n"
    );
}

#[gpui::test]
async fn taking_the_layout_away_returns_to_markdown(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown(
        "Text\n\n<p align=\"center\"><img src=\"images/photo.png\" alt=\"写真\" width=\"300\">ˇ</p>\n\nMore\n",
        cx,
    )
    .await;
    let dialog = open_dialog(&opened, cx);

    set_field(&dialog, |dialog| dialog.width_field.clone(), "", cx);
    dialog.update(cx, |dialog, cx| {
        dialog.alignment = None;
        cx.notify();
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "Text\n\n![写真](images/photo.png)\n\nMore\n"
    );
}

#[gpui::test]
async fn an_unchanged_path_is_written_back_as_it_was(cx: &mut TestAppContext) {
    // An absolute path outside the project would be offered for copying if it
    // were picked now; an edit that only resizes must leave it alone.
    let marked = format!(
        "![外](<{}>ˇ)\n",
        path!("/outside/pic.png").replace('\\', "/")
    );
    let (opened, cx) = open_markdown(&marked, cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(&dialog, |dialog| dialog.width_field.clone(), "80", cx);
    let copy_shown = cx.update(|_window, cx| {
        let dialog = dialog.read(cx);
        match dialog.resolved(cx) {
            ResolvedSource::File(file) => dialog.copy_offered(&file, cx),
            _ => false,
        }
    });
    assert!(!copy_shown);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    let written = path!("/outside/pic.png").replace('\\', "/");
    assert_eq!(
        buffer_text(&opened, cx),
        format!("<img src=\"{written}\" alt=\"外\" width=\"80\">\n")
    );
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/pic.png")))
            .await
    );
}

#[gpui::test]
async fn a_file_from_outside_the_project_is_copied_next_to_the_document(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    opened
        .fs
        .insert_file(path!("/project/docs/images/pic.png"), b"taken".to_vec())
        .await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    let copy_shown = cx.update(|_window, cx| {
        let dialog = dialog.read(cx);
        match dialog.resolved(cx) {
            ResolvedSource::File(file) => dialog.copy_offered(&file, cx),
            _ => false,
        }
    });
    assert!(
        copy_shown,
        "a file outside the worktree is offered for copying"
    );
    assert!(dialog.read_with(cx, |dialog, _| dialog.copy_into_images));

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "![pic](images/pic-1.png)\n");
    assert_eq!(
        opened
            .fs
            .load(Path::new(path!("/project/docs/images/pic-1.png")))
            .await
            .ok()
            .as_deref(),
        Some("outside")
    );
}

#[gpui::test]
async fn declining_the_copy_writes_a_relative_path(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    dialog.update(cx, |dialog, cx| {
        dialog.copy_into_images = false;
        cx.notify();
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "![pic](../../outside/pic.png)\n");
}

#[gpui::test]
async fn a_selection_becomes_the_alt_text_and_is_replaced(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("See «the logoˇ» here\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    assert_eq!(
        field_text(&dialog, |dialog| dialog.alt_field.clone(), cx),
        "the logo"
    );
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "../assets/logo.svg",
        cx,
    );
    assert_eq!(
        field_text(&dialog, |dialog| dialog.alt_field.clone(), cx),
        "the logo",
        "text the user already has is not replaced by the file name"
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "See ![the logo](../assets/logo.svg) here\n"
    );
}

#[gpui::test]
async fn an_invalid_width_is_refused_with_the_reason(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    set_field(&dialog, |dialog| dialog.width_field.clone(), "wide", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert_eq!(buffer_text(&opened, cx), "\n");
    assert!(dialog_is_open(&opened, cx));
    let message = dialog.read_with(cx, |dialog, _| dialog.message.clone());
    assert_eq!(
        message.as_deref(),
        Some("Enter the width as a number or a number with %, such as 300 or 50%.")
    );

    // An empty path is refused the same way.
    set_field(&dialog, |dialog| dialog.width_field.clone(), "", cx);
    set_field(&dialog, |dialog| dialog.path_field.clone(), "", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "\n");
    let message = dialog.read_with(cx, |dialog, _| dialog.message.clone());
    assert_eq!(message.as_deref(), Some("Specify an image file."));
}

#[gpui::test]
async fn escape_closes_without_writing(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("Keep ˇthis\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();

    assert!(!dialog_is_open(&opened, cx));
    assert_eq!(buffer_text(&opened, cx), "Keep this\n");
}

#[gpui::test]
async fn a_change_underneath_the_dialog_is_not_overwritten(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("![a](images/phˇoto.png)\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    opened.editor.update_in(cx, |editor, window, cx| {
        editor.set_text("![b](images/other.png)\n", window, cx);
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert_eq!(buffer_text(&opened, cx), "![b](images/other.png)\n");
    assert!(
        dialog
            .read_with(cx, |dialog, _| dialog.message.clone())
            .is_some(),
        "the dialog should say why it did not write"
    );
}

#[gpui::test]
async fn a_refused_write_copies_nothing(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("![a](images/phˇoto.png)\n", cx).await;
    let dialog = open_dialog(&opened, cx);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    opened.editor.update_in(cx, |editor, window, cx| {
        editor.set_text("![b](images/other.png)\n", window, cx);
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert_eq!(buffer_text(&opened, cx), "![b](images/other.png)\n");
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/pic.png")))
            .await,
        "nothing is copied for an image that is not going to be written"
    );
    assert!(
        dialog
            .read_with(cx, |dialog, _| dialog.message.clone())
            .is_some()
    );
}

#[gpui::test]
async fn a_write_refused_after_copying_takes_the_copy_away(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("«写真ˇ»\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    dialog.update_in(cx, |dialog, window, cx| dialog.submit(window, cx));
    assert!(dialog.read_with(cx, |dialog, _| dialog.copying));
    opened.editor.update_in(cx, |editor, window, cx| {
        editor.set_text("changed\n", window, cx);
    });
    cx.run_until_parked();

    assert_eq!(buffer_text(&opened, cx), "changed\n");
    assert!(
        dialog
            .read_with(cx, |dialog, _| dialog.message.clone())
            .is_some()
    );
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/pic.png")))
            .await,
        "a copy nothing links to is taken away again"
    );
    assert!(
        opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/photo.png")))
            .await
    );
}

#[gpui::test]
async fn a_path_typed_for_a_remote_document_is_not_copied(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    dialog.update(cx, |dialog, _| dialog.location.is_local = false);

    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    let copy_shown = cx.update(|_window, cx| {
        let dialog = dialog.read(cx);
        match dialog.resolved(cx) {
            ResolvedSource::File(file) => dialog.copy_offered(&file, cx),
            _ => false,
        }
    });
    assert!(!copy_shown, "nothing is copied across a remote connection");

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "![pic](../../outside/pic.png)\n");
}

#[gpui::test]
async fn a_file_picked_for_a_remote_document_is_always_copied(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    dialog.update(cx, |dialog, _| dialog.location.is_local = false);

    let picked = PathBuf::from(path!("/outside/pic.png"));
    dialog.update_in(cx, |dialog, window, cx| {
        dialog.use_picked_file(&picked, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        field_text(&dialog, |dialog| dialog.path_field.clone(), cx),
        path!("/outside/pic.png")
    );
    let (copies, details) = cx.update(|_window, cx| {
        let dialog = dialog.read(cx);
        let copies = dialog.validate(cx).is_ok_and(|plan| plan.copy.is_some());
        (copies, dialog.details.clone())
    });
    assert!(
        copies,
        "the host cannot read this machine's file, so it is copied"
    );
    assert!(
        matches!(
            details,
            SourceDetails::Found {
                preview: Some(PreviewSource::Local(_)),
                ..
            }
        ),
        "the thumbnail comes from this machine, got {details:?}"
    );

    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(buffer_text(&opened, cx), "![pic](images/pic.png)\n");
    assert_eq!(
        opened
            .fs
            .load(Path::new(path!("/project/docs/images/pic.png")))
            .await
            .ok()
            .as_deref(),
        Some("outside")
    );
}

#[gpui::test]
async fn editing_the_path_after_a_remote_pick_reads_it_as_a_remote_path(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    dialog.update(cx, |dialog, _| dialog.location.is_local = false);
    let picked = PathBuf::from(path!("/outside/pic.png"));
    dialog.update_in(cx, |dialog, window, cx| {
        dialog.use_picked_file(&picked, window, cx)
    });
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/photo.png",
        cx,
    );
    let copies = cx.update(|_window, cx| {
        let dialog = dialog.read(cx);
        dialog.picked_local(cx).is_some()
            || dialog.validate(cx).is_ok_and(|plan| plan.copy.is_some())
    });
    assert!(!copies);
}

#[gpui::test]
async fn closing_the_dialog_while_copying_leaves_no_copy_behind(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    dialog.update_in(cx, |dialog, window, cx| dialog.submit(window, cx));
    assert!(dialog.read_with(cx, |dialog, _| dialog.copying));
    cx.dispatch_action(menu::Cancel);
    drop(dialog);
    cx.run_until_parked();

    assert!(!dialog_is_open(&opened, cx));
    assert_eq!(buffer_text(&opened, cx), "\n");
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/pic.png")))
            .await,
        "a copy nothing links to is taken away again"
    );
}

/// A document opened on its own copies a picture into the `images` folder
/// beside it, as one in an open folder does.
#[gpui::test]
async fn a_document_opened_on_its_own_copies_beside_itself(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/notes"),
        json!({"memo.md": "", "images": {"pic.png": "taken"}}),
    )
    .await;
    fs.insert_tree(path!("/outside"), json!({"pic.png": "outside"}))
        .await;
    let (opened, cx) = open_in_project(
        app_state,
        fs,
        path!("/notes/memo.md"),
        RelPath::empty(),
        vec![0..0],
        cx,
    )
    .await;
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        path!("/outside/pic.png"),
        cx,
    );
    let (copy_shown, warnings) = cx.update(|_window, cx| {
        let dialog = dialog.read(cx);
        let copy_shown = match dialog.resolved(cx) {
            ResolvedSource::File(file) => dialog.copy_offered(&file, cx),
            _ => false,
        };
        (copy_shown, dialog.warnings(cx))
    });
    assert!(copy_shown);
    assert_eq!(warnings, Vec::<SharedString>::new());
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert!(!dialog_is_open(&opened, cx));
    assert_eq!(buffer_text(&opened, cx), "![pic](images/pic-1.png)");
    assert_eq!(
        opened
            .fs
            .load(Path::new(path!("/notes/images/pic-1.png")))
            .await
            .ok()
            .as_deref(),
        Some("outside")
    );
    assert_eq!(
        opened
            .fs
            .load(Path::new(path!("/notes/images/pic.png")))
            .await
            .ok()
            .as_deref(),
        Some("taken")
    );
}

#[gpui::test]
async fn images_in_code_and_comments_are_not_edited(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    for marked in [
        "`![a](images/phˇoto.png)`\n",
        "```\n![a](images/phˇoto.png)\n```\n",
        "~~~md\n<img src=\"images/phˇoto.png\">\n~~~\n",
        "<!-- ![a](images/phˇoto.png) -->\n",
        "Text <!-- a\n![a](images/phˇoto.png) --> more\n",
    ] {
        reset_document(&opened, marked, cx);
        let dialog = open_dialog(&opened, cx);
        assert!(
            !dialog.read_with(cx, |dialog, _| dialog.is_editing()),
            "{marked:?}"
        );
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
    }
    reset_document(&opened, "`code` ![a](images/phˇoto.png)\n", cx);
    let dialog = open_dialog(&opened, cx);
    assert!(dialog.read_with(cx, |dialog, _| dialog.is_editing()));
}

#[gpui::test]
async fn an_aligned_image_inside_emphasis_goes_after_the_paragraph(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let center = Some(ImageAlignment::Center);

    // At the end of the line, but in the middle of the emphasis.
    let (text, _) = write_with_alignment(&opened, "*強調の途中ˇ\nつづき*\n\nNext\n", center, cx);
    assert_eq!(
        text,
        format!("*強調の途中\nつづき*\n\n{CENTERED_PHOTO}\n\nNext\n")
    );
    // At the start of a line, in the middle of a link's text.
    let (text, _) = write_with_alignment(
        &opened,
        "[リンク\nˇ文字](https://example.com)\n",
        center,
        cx,
    );
    assert_eq!(
        text,
        format!("[リンク\n文字](https://example.com)\n\n{CENTERED_PHOTO}\n")
    );
    // After the emphasis has closed, the line ends where the cursor is.
    let (text, _) = write_with_alignment(&opened, "**太字**ˇ\n\nNext\n", center, cx);
    assert_eq!(text, format!("**太字**\n\n{CENTERED_PHOTO}\n\nNext\n"));
}

#[gpui::test]
async fn a_one_line_container_stays_on_one_line(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown(
        "Text\n\n<p align=\"center\"><img src=\"images/photo.png\">ˇ</p>\n\nMore\n",
        cx,
    )
    .await;
    let dialog = open_dialog(&opened, cx);
    set_field(&dialog, |dialog| dialog.width_field.clone(), "120", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "Text\n\n<p align=\"center\"><img src=\"images/photo.png\" width=\"120\"></p>\n\nMore\n"
    );
}

#[gpui::test]
async fn a_reference_image_changes_its_definition(cx: &mut TestAppContext) {
    let (opened, cx) =
        open_markdown("![ロゴ][logo]ˇ\n\n[logo]: images/photo.png \"題\"\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    assert!(dialog.read_with(cx, |dialog, _| dialog.is_editing()));
    assert_eq!(
        field_text(&dialog, |dialog| dialog.path_field.clone(), cx),
        "images/photo.png"
    );
    assert_eq!(
        field_text(&dialog, |dialog| dialog.alt_field.clone(), cx),
        "ロゴ"
    );
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/real.png",
        cx,
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "![ロゴ][logo]\n\n[logo]: images/real.png \"題\"\n"
    );

    // The definition may come first; an alt text alone leaves it as it is.
    reset_document(&opened, "[logo]: <images/a b.png>\n\n![ロゴ][logo]ˇ\n", cx);
    let dialog = open_dialog(&opened, cx);
    assert_eq!(
        field_text(&dialog, |dialog| dialog.path_field.clone(), cx),
        "images/a b.png"
    );
    set_field(&dialog, |dialog| dialog.alt_field.clone(), "新しい", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "[logo]: <images/a b.png>\n\n![新しい][logo]\n"
    );

    reset_document(&opened, "[logo]: images/photo.png\n\n![ロゴ][logo]ˇ\n", cx);
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/new one.png",
        cx,
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "[logo]: images/new%20one.png\n\n![ロゴ][logo]\n"
    );
    let cursor = opened.editor.update(cx, |editor, cx| {
        editor
            .selections
            .newest::<MultiBufferOffset>(&editor.display_snapshot(cx))
            .head()
            .0
    });
    assert_eq!(
        cursor,
        buffer_text(&opened, cx).len() - 1,
        "the cursor ends up after the image, past the longer definition"
    );
}

#[gpui::test]
async fn a_shared_reference_definition_is_rewritten_only_when_asked(cx: &mut TestAppContext) {
    let marked = "![ロゴ][logo]ˇ and [link][logo]\n\n[logo]: images/photo.png \"題\"\n";
    let (opened, cx) = open_markdown(marked, cx).await;
    let dialog = open_dialog(&opened, cx);
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.shared_definition_uses()),
        Some(2)
    );
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/real.png",
        cx,
    );
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "![ロゴ](images/real.png \"題\") and [link][logo]\n\n[logo]: images/photo.png \"題\"\n",
        "by default only this image changes, written inline"
    );

    reset_document(&opened, marked, cx);
    let dialog = open_dialog(&opened, cx);
    set_field(
        &dialog,
        |dialog| dialog.path_field.clone(),
        "images/real.png",
        cx,
    );
    dialog.update(cx, |dialog, cx| {
        dialog.rewrite_shared_definition = true;
        cx.notify();
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "![ロゴ][logo] and [link][logo]\n\n[logo]: images/real.png \"題\"\n"
    );
}

#[gpui::test]
async fn short_reference_forms_keep_their_definition(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("![logo][]ˇ\n\n[logo]: images/photo.png\n", cx).await;
    let dialog = open_dialog(&opened, cx);
    set_field(&dialog, |dialog| dialog.alt_field.clone(), "新しい", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "![新しい][logo]\n\n[logo]: images/photo.png\n",
        "a new alt text would be a new label, so the label is written out"
    );

    // A size cannot be written in the reference form, so the image becomes
    // an `<img>` and the definition stays for whatever else uses it.
    reset_document(&opened, "![logo]ˇ\n\n[logo]: images/photo.png\n", cx);
    let dialog = open_dialog(&opened, cx);
    assert!(dialog.read_with(cx, |dialog, _| dialog.is_editing()));
    set_field(&dialog, |dialog| dialog.width_field.clone(), "10", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "<img src=\"images/photo.png\" alt=\"logo\" width=\"10\">\n\n[logo]: images/photo.png\n"
    );
}

#[test]
fn a_linux_host_is_read_with_its_own_path_rules() {
    // A Windows client editing a document on a Linux host: the host's
    // file names differ by case alone and its paths use `/`.
    let host = DocumentLocation {
        directory: Some("/home/u/docs".into()),
        root: Some("/home/u/docs".into()),
        path_style: PathStyle::Unix,
        is_local: false,
    };
    let ResolvedSource::File(other_case) = resolve_source("/home/u/Docs/a.png", &host) else {
        panic!("an absolute path on the host is a file");
    };
    assert_eq!(other_case.relative.as_deref(), Some("../Docs/a.png"));
    assert!(
        !other_case.inside_root,
        "Docs is not docs on a case-sensitive host"
    );
    let ResolvedSource::File(typed) = resolve_source(r"images\a.png", &host) else {
        panic!("a relative path is a file");
    };
    assert_eq!(
        typed.relative.as_deref(),
        Some(r"images\a.png"),
        "a backslash is part of a file name on the host, not a separator"
    );
    assert!(!PathStyle::Unix.is_absolute(r"C:\Users\a.png"));
    assert_eq!(
        relative_path("/home/u/docs", "/home/u/docs/Images/A.png", PathStyle::Unix).as_deref(),
        Some("Images/A.png")
    );
}

#[gpui::test]
async fn stores_a_picture_next_to_the_document_without_overwriting(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let document = cx
        .update(|_window, cx| document_project_path(&opened.editor, cx))
        .expect("the document is saved");
    opened
        .fs
        .create_symlink(
            Path::new(path!("/project/docs/images/shot.png")),
            PathBuf::from(path!("/nowhere/shot.png")),
        )
        .await
        .expect("the link is made");
    cx.run_until_parked();

    let store = |preferred: &str, bytes: &[u8], cx: &mut VisualTestContext| {
        let task = cx.update(|_window, cx| {
            image_files::store_image_next_to(
                &project,
                &document,
                ImageData::Bytes {
                    bytes: bytes.to_vec(),
                    extension: "png".into(),
                },
                (!preferred.is_empty()).then(|| preferred.to_string()),
                cx,
            )
        });
        task
    };
    let stored_name = |stored: Result<ProjectPath>| {
        stored
            .map(|stored| stored.path.as_unix_str().to_string())
            .unwrap_or_else(|error| panic!("the picture should be stored: {error:#}"))
    };

    assert_eq!(
        stored_name(store("photo.png", b"first", cx).await),
        "docs/images/photo-1.png"
    );
    assert_eq!(
        stored_name(store("photo.png", b"second", cx).await),
        "docs/images/photo-2.png"
    );
    let load = |path: &str| {
        let fs = opened.fs.clone();
        let path = PathBuf::from(path);
        async move { fs.load(&path).await.ok() }
    };
    assert_eq!(
        load(path!("/project/docs/images/photo.png"))
            .await
            .as_deref(),
        Some("not really a png"),
        "what was there is not overwritten"
    );
    assert_eq!(
        load(path!("/project/docs/images/photo-2.png"))
            .await
            .as_deref(),
        Some("second")
    );

    assert_eq!(
        stored_name(store("shot.png", b"shot", cx).await),
        "docs/images/shot-1.png",
        "a link that leads nowhere takes its name"
    );
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/nowhere/shot.png")))
            .await,
        "nothing is written where the link points"
    );

    assert_eq!(
        stored_name(store("", b"pasted", cx).await),
        "docs/images/image.png"
    );
    assert_eq!(
        stored_name(store("a/b:c?.PNG", b"x", cx).await),
        "docs/images/a-b-c-.PNG"
    );
    assert_eq!(
        stored_name(store("スクリーンショット", b"x", cx).await),
        "docs/images/スクリーンショット.png",
        "a name without an extension takes the picture's"
    );

    let too_large = store(
        "large.png",
        &vec![0; image_files::MAXIMUM_IMAGE_BYTES as usize + 1],
        cx,
    )
    .await;
    let error = too_large.expect_err("a picture over the limit is refused");
    assert!(error.to_string().contains("too large"), "{error:#}");
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/large.png")))
            .await
    );

    let local = cx.update(|_window, cx| {
        image_files::store_image_next_to(
            &project,
            &document,
            ImageData::LocalFile(PathBuf::from(path!("/outside/pic.png"))),
            None,
            cx,
        )
    });
    assert_eq!(stored_name(local.await), "docs/images/pic.png");

    let alone = cx.update(|_window, cx| {
        image_files::store_image_next_to(
            &project,
            &ProjectPath {
                worktree_id: document.worktree_id,
                path: RelPath::empty_arc(),
            },
            ImageData::Bytes {
                bytes: b"x".to_vec(),
                extension: "png".into(),
            },
            None,
            cx,
        )
    });
    let error = alone
        .await
        .expect_err("a file opened on its own has no folder to store into");
    assert!(error.to_string().contains("opened on its own"), "{error:#}");
}

#[gpui::test]
async fn a_dropped_store_takes_its_picture_away_again(cx: &mut TestAppContext) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    let project = opened
        .workspace
        .read_with(cx, |workspace, _| workspace.project().clone());
    let document = cx
        .update(|_window, cx| document_project_path(&opened.editor, cx))
        .expect("the document is saved");
    let task = cx.update(|_window, cx| {
        image_files::store_image_next_to(
            &project,
            &document,
            ImageData::Bytes {
                bytes: b"abandoned".to_vec(),
                extension: "png".into(),
            },
            Some("abandoned.png".into()),
            cx,
        )
    });
    drop(task);
    cx.run_until_parked();
    assert!(
        !opened
            .fs
            .is_file(Path::new(path!("/project/docs/images/abandoned.png")))
            .await
    );
    let names = opened
        .fs
        .files()
        .into_iter()
        .filter(|path| path.to_string_lossy().contains("abandoned"))
        .collect::<Vec<_>>();
    assert_eq!(names, Vec::<PathBuf>::new(), "no empty placeholder is left");
}

#[gpui::test]
fn writes_links_from_the_document_to_a_stored_picture(cx: &mut App) {
    let worktree_id = project::WorktreeId::from_usize(1);
    let at = |path: &str| ProjectPath {
        worktree_id,
        path: RelPath::from_unix_str(path)
            .expect("a relative path")
            .into_arc(),
    };
    let document = at("docs/readme.md");
    assert_eq!(
        image_files::link_destination(&document, &at("docs/images/a b(1).png"), true, cx)
            .as_deref(),
        Some("images/a%20b%281%29.png")
    );
    assert_eq!(
        image_files::link_destination(&document, &at("docs/images/a b(1).png"), false, cx)
            .as_deref(),
        Some("images/a%20b(1).png")
    );
    assert_eq!(
        image_files::link_destination(&document, &at("assets/x&y #1.png"), false, cx).as_deref(),
        Some("../assets/x&amp;y%20%231.png"),
        "an attribute's text is escaped for its quotes"
    );
    assert_eq!(
        image_files::link_destination(&document, &at("docs/images/say \"hi\".png"), false, cx)
            .as_deref(),
        Some("images/say%20%22hi%22.png")
    );
    assert_eq!(
        image_files::link_destination(&document, &at("docs/images/say \"hi\".png"), true, cx)
            .as_deref(),
        Some("images/say%20\"hi\".png")
    );
    assert_eq!(
        image_files::link_destination(&at("readme.md"), &at("images/写真.png"), true, cx)
            .as_deref(),
        Some("images/写真.png")
    );
    let elsewhere = ProjectPath {
        worktree_id: project::WorktreeId::from_usize(2),
        path: at("images/a.png").path,
    };
    assert_eq!(
        image_files::link_destination(&document, &elsewhere, true, cx),
        None
    );
    let alone = ProjectPath {
        worktree_id,
        path: RelPath::empty_arc(),
    };
    assert_eq!(
        image_files::link_destination(&alone, &at("images/a.png"), true, cx).as_deref(),
        Some("images/a.png"),
        "a document opened on its own has its pictures stored beside it"
    );
    assert_eq!(
        image_files::link_destination(&alone, &alone, true, cx),
        None
    );
}

/// The parser reads a definition across the markers of the quote or list it
/// runs on in, as the dialog has to: the path on the line after the label is
/// the definition's, and a title there is kept when the path changes.
#[gpui::test]
async fn a_reference_definition_on_several_lines_is_read_as_the_parser_reads_it(
    cx: &mut TestAppContext,
) {
    let (opened, cx) = open_markdown("ˇ\n", cx).await;
    for (marked, expected) in [
        (
            "> ![a][r]ˇ\n>\n> [r]: images/photo.png\n> \"題\"\n",
            "> ![a][r]\n>\n> [r]: images/real.png \"題\"\n",
        ),
        (
            "> ![a][x]ˇ\n>\n> [x]:\n> images/photo.png\n",
            "> ![a][x]\n>\n> [x]: images/real.png\n",
        ),
        (
            "> ![a][x]ˇ\n>\n> [x]: images/photo.png \"one\n> two\"\n",
            "> ![a][x]\n>\n> [x]: images/real.png \"one two\"\n",
        ),
        (
            "- ![a][r]ˇ\n\n  [r]: images/photo.png\n  'title'\n",
            "- ![a][r]\n\n  [r]: images/real.png \"title\"\n",
        ),
        // Labels match regardless of case and runs of spaces, and the first
        // of two definitions with one label is the one used.
        (
            "![a][My  LOGO]ˇ\n\n[my logo]: images/photo.png\n",
            "![a][My  LOGO]\n\n[my logo]: images/real.png\n",
        ),
        (
            "![a][x]ˇ\n\n[x]: images/photo.png\n[x]: images/other.png\n",
            "![a][x]\n\n[x]: images/real.png\n[x]: images/other.png\n",
        ),
        (
            "![図][図１]ˇ\n\n[図１]: images/photo.png (括弧)\n",
            "![図][図１]\n\n[図１]: images/real.png \"括弧\"\n",
        ),
    ] {
        reset_document(&opened, marked, cx);
        let dialog = open_dialog(&opened, cx);
        assert_eq!(
            field_text(&dialog, |dialog| dialog.path_field.clone(), cx),
            "images/photo.png",
            "{marked:?}"
        );
        set_field(
            &dialog,
            |dialog| dialog.path_field.clone(),
            "images/real.png",
            cx,
        );
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(buffer_text(&opened, cx), expected, "{marked:?}");
    }

    // With the path left as it was, the image written as HTML takes the path
    // the definition holds, not the marker of the line it is on.
    reset_document(&opened, "> ![a][x]ˇ\n>\n> [x]:\n> images/photo.png\n", cx);
    let dialog = open_dialog(&opened, cx);
    set_field(&dialog, |dialog| dialog.width_field.clone(), "10", cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(
        buffer_text(&opened, cx),
        "> <img src=\"images/photo.png\" alt=\"a\" width=\"10\">\n>\n> [x]:\n> images/photo.png\n"
    );
}
