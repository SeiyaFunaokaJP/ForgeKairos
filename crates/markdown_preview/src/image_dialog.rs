//! The modal behind `markdown::InsertImage`.
//!
//! An image with a size or an alignment has no Markdown spelling, so it has to
//! be written as HTML, and HTML inside Markdown has traps of its own: the path
//! must be relative to the document and percent-encoded, attribute values
//! escaped, and a `<div align>` block kept free of blank lines while being set
//! apart from the paragraphs around it by them. The modal asks for the picture,
//! its text and its layout, and writes whichever of `![alt](path)`, `<img>` and
//! `<div align><img></div>` those call for. Opened on an image that is already
//! there, it reads that image back into the form and replaces it, keeping the
//! attributes it has no field for.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context as _, Result, anyhow};
use editor::actions::{Backtab, MoveToEnd, Tab};
use editor::{Editor, EditorEvent, MultiBufferOffset};
use fs::{CopyOptions, Fs};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ImageSource, ObjectFit, PathPromptOptions, Resource, ScrollHandle, Subscription, Task,
    TextStyleRefinement, WeakEntity, Window, img,
};
use language::{Anchor, BufferSnapshot, ToOffset as _};
use project::{ImageItem, Project, ProjectPath, image_store};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    Checkbox, HighlightedLabel, Modal, ModalFooter, ModalHeader, Section, ToggleButtonGroup,
    ToggleButtonGroupStyle, ToggleButtonSimple, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use util::paths::PathStyle;
use workspace::{ModalView, Workspace};
use zed_actions::editor::{MoveDown, MoveUp};

use crate::InsertImage;
use crate::markdown_preview_view::MarkdownPreviewView;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &InsertImage, window, cx| {
        let Some(editor) =
            MarkdownPreviewView::resolve_active_item_as_markdown_editor(workspace, cx)
        else {
            // Reachable from the command palette with anything in front, where
            // returning in silence leaves the user guessing.
            workspace.show_error(
                anyhow!("Markdown のエディターを選んでから実行してください。"),
                cx,
            );
            return;
        };
        // The modal writes through the editor, which drops the edit when the
        // buffer cannot take it; saying so now beats a form that goes nowhere.
        if editor.read(cx).read_only(cx) {
            workspace.show_error(
                anyhow!("このバッファーは読み取り専用のため、画像を挿入できません。"),
                cx,
            );
            return;
        }
        let Some(target) = editor.update(cx, |editor, cx| image_target(editor, cx)) else {
            return;
        };
        let project = workspace.project().clone();
        let location = document_location(&editor, &project, cx);
        workspace.toggle_modal(window, cx, move |window, cx| {
            ImageDialog::new(editor.downgrade(), project, location, target, window, cx)
        });
    });
}

/// Extensions the file list offers and the file picker accepts.
const IMAGE_EXTENSIONS: [&str; 9] = [
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "avif",
];

/// How many matches the file list draws. The rest are a keystroke away, and
/// a list of thousands of rows is no easier to pick from than a filter.
const MAXIMUM_LISTED_CANDIDATES: usize = 200;

/// How much of the window's height the form may take before it scrolls, which
/// leaves room for the header and the footer in a short window.
const MAXIMUM_FORM_HEIGHT_FRACTION: f32 = 0.6;

fn has_image_extension(path: &str) -> bool {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file_name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageAlignment {
    Left,
    Center,
    Right,
}

impl ImageAlignment {
    fn attribute_value(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }

    fn from_attribute(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// The attributes the form has a field for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KnownAttribute {
    Source,
    Alt,
    Width,
    Height,
    Title,
    Style,
    Align,
}

/// Where an attribute sat in the tag it was read from, so that writing the tag
/// back keeps the order its author chose.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AttributeSlot {
    Known(KnownAttribute),
    /// An attribute the form has no field for, written back exactly as it was
    /// spelled.
    Verbatim {
        name: String,
        source: String,
    },
}

/// The `<div align>` or `<p align>` an image was found in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ImageContainer {
    /// The tag name as written, so `<DIV>` closes as `</DIV>`.
    tag: String,
    slots: Vec<AttributeSlot>,
}

impl ImageContainer {
    fn has_attributes_of_its_own(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot, AttributeSlot::Verbatim { .. }))
    }
}

/// One image as the dialog reads it from the buffer and writes it back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageMarkup {
    /// A path with its percent escapes decoded, as the user reads it, or a URL
    /// exactly as it was written.
    pub destination: String,
    /// The destination as the document spelled it, for as long as the path
    /// field still holds it. Encoding the decoded form again would turn a
    /// `#gh-dark-mode-only` or `?raw=true` into part of the file name and
    /// rewrite escapes the author chose, so it is written back as it was.
    written_destination: Option<String>,
    pub alt: String,
    pub title: Option<String>,
    /// A number of pixels or a percentage, as [`parse_length`] accepts it.
    pub width: Option<String>,
    pub height: Option<String>,
    pub alignment: Option<ImageAlignment>,
    /// The `<img>`'s `style` without the width and height the fields took over.
    style: Option<String>,
    image_slots: Vec<AttributeSlot>,
    self_closing: bool,
    container: Option<ImageContainer>,
    /// Whether the image sits inside an HTML block, where Markdown is not read
    /// and only an `<img>` shows as a picture.
    keep_html: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RenderedImage {
    pub text: String,
    /// Whether the text is an HTML block that must sit on lines of its own.
    pub is_block: bool,
}

impl ImageMarkup {
    /// The markup for this image. `indent` is put in front of every line after
    /// the first, which is what keeps a block inside the list item it is in.
    pub(crate) fn render(&self, indent: &str) -> RenderedImage {
        let in_container = self.is_written_in_a_container();
        if !in_container && !self.needs_html() {
            return RenderedImage {
                text: self.markdown_image(),
                is_block: false,
            };
        }

        let image = self.image_tag();
        if !in_container {
            return RenderedImage {
                text: image,
                is_block: false,
            };
        }
        let container = self.container.as_ref();
        let tag = container.map_or("div", |container| container.tag.as_str());
        RenderedImage {
            text: format!(
                "{}\n{indent}  {image}\n{indent}</{tag}>",
                self.container_open_tag(tag, container)
            ),
            is_block: true,
        }
    }

    /// Whether anything the Markdown form cannot spell is set.
    fn needs_html(&self) -> bool {
        self.keep_html
            || self.width.is_some()
            || self.height.is_some()
            || self.style.is_some()
            || self
                .image_slots
                .iter()
                .any(|slot| matches!(slot, AttributeSlot::Verbatim { .. }))
    }

    /// Whether the image is written inside a container. One whose only
    /// attribute was the alignment that has now been taken away is dropped
    /// with it; one that carries attributes of its own stays, since dropping it
    /// would drop those.
    fn is_written_in_a_container(&self) -> bool {
        self.alignment.is_some()
            || self
                .container
                .as_ref()
                .is_some_and(ImageContainer::has_attributes_of_its_own)
    }

    fn encoded_destination(&self, in_markdown: bool) -> String {
        match &self.written_destination {
            Some(written) => encode_as_written(written, in_markdown),
            None => encode_destination(&self.destination, in_markdown),
        }
    }

    fn markdown_image(&self) -> String {
        let mut text = format!(
            "![{}]({}",
            escape_markdown(&self.alt, &['[', ']']),
            self.encoded_destination(true)
        );
        if let Some(title) = &self.title {
            text.push_str(" \"");
            text.push_str(&escape_markdown(title, &['"']));
            text.push('"');
        }
        text.push(')');
        text
    }

    fn image_tag(&self) -> String {
        let mut attributes = Vec::new();
        let mut written = Vec::new();
        for slot in &self.image_slots {
            match slot {
                AttributeSlot::Known(kind) => {
                    written.push(*kind);
                    attributes.extend(self.image_attribute(*kind));
                }
                AttributeSlot::Verbatim { name, source } => {
                    // An unreadable `width="auto"` stays as it was, until a
                    // width typed into the field takes its place.
                    let replaced = (name == "width" && self.width.is_some())
                        || (name == "height" && self.height.is_some());
                    if !replaced {
                        attributes.push(source.clone());
                    }
                }
            }
        }
        for kind in [
            KnownAttribute::Source,
            KnownAttribute::Alt,
            KnownAttribute::Width,
            KnownAttribute::Height,
            KnownAttribute::Title,
            KnownAttribute::Style,
        ] {
            if !written.contains(&kind) {
                attributes.extend(self.image_attribute(kind));
            }
        }
        let end = if self.self_closing { " />" } else { ">" };
        format!("<img {}{end}", attributes.join(" "))
    }

    fn image_attribute(&self, kind: KnownAttribute) -> Option<String> {
        let attribute = |name: &str, value: &str| format!("{name}=\"{}\"", escape_attribute(value));
        match kind {
            KnownAttribute::Source => Some(attribute("src", &self.encoded_destination(false))),
            // An empty `alt` is what marks an image as decoration, so it is
            // written even then.
            KnownAttribute::Alt => Some(attribute("alt", &self.alt)),
            KnownAttribute::Width => self.width.as_deref().map(|width| attribute("width", width)),
            KnownAttribute::Height => self
                .height
                .as_deref()
                .map(|height| attribute("height", height)),
            KnownAttribute::Title => self.title.as_deref().map(|title| attribute("title", title)),
            KnownAttribute::Style => self.style.as_deref().map(|style| attribute("style", style)),
            KnownAttribute::Align => None,
        }
    }

    fn container_open_tag(&self, tag: &str, container: Option<&ImageContainer>) -> String {
        let alignment = self
            .alignment
            .map(|alignment| format!("align=\"{}\"", alignment.attribute_value()));
        let mut attributes = Vec::new();
        let mut wrote_alignment = false;
        for slot in container.map_or(&[][..], |container| container.slots.as_slice()) {
            match slot {
                AttributeSlot::Known(KnownAttribute::Align) => {
                    wrote_alignment = true;
                    attributes.extend(alignment.clone());
                }
                AttributeSlot::Known(_) => {}
                // An `align` the form could not read, such as `justify`, gives
                // way to the one picked in the form.
                AttributeSlot::Verbatim { name, .. }
                    if name == "align" && self.alignment.is_some() => {}
                AttributeSlot::Verbatim { source, .. } => attributes.push(source.clone()),
            }
        }
        if !wrote_alignment && let Some(alignment) = alignment {
            attributes.insert(0, alignment);
        }
        let mut text = format!("<{tag}");
        for attribute in attributes {
            text.push(' ');
            text.push_str(&attribute);
        }
        text.push('>');
        text
    }
}

/// Percent-encodes what a destination cannot hold as it is. A URL keeps its
/// own escapes; a path is encoded from its decoded form, `%` included.
fn encode_destination(destination: &str, in_markdown: bool) -> String {
    if is_url(destination) {
        return encode_as_written(destination, in_markdown);
    }
    let mut encoded = String::with_capacity(destination.len());
    for character in destination.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            '#' => encoded.push_str("%23"),
            '?' => encoded.push_str("%3F"),
            '\\' => encoded.push_str("%5C"),
            character => push_structural(&mut encoded, character, in_markdown),
        }
    }
    encoded
}

/// Encodes only what would end the destination or the Markdown around it,
/// keeping the escapes, query and fragment a URL or a written path already has.
fn encode_as_written(destination: &str, in_markdown: bool) -> String {
    let mut encoded = String::with_capacity(destination.len());
    let mut characters = destination.chars().peekable();
    while let Some(character) = characters.next() {
        // Before punctuation a backslash would escape it in Markdown, where it
        // stood for itself in the attribute it came from.
        if character == '\\'
            && in_markdown
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_punctuation())
        {
            encoded.push_str("%5C");
        } else {
            push_structural(&mut encoded, character, in_markdown);
        }
    }
    encoded
}

fn push_structural(encoded: &mut String, character: char, in_markdown: bool) {
    match character {
        ' ' => encoded.push_str("%20"),
        // A line break or a tab ends a Markdown destination, and a blank line
        // would split the HTML block.
        character if character.is_ascii_control() => {
            encoded.push_str(&format!("%{:02X}", u32::from(character)));
        }
        '(' if in_markdown => encoded.push_str("%28"),
        ')' if in_markdown => encoded.push_str("%29"),
        '<' if in_markdown => encoded.push_str("%3C"),
        '>' if in_markdown => encoded.push_str("%3E"),
        character => encoded.push(character),
    }
}

fn decode_destination(destination: &str) -> String {
    if is_url(destination) {
        return destination.to_string();
    }
    urlencoding::decode(destination)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| destination.to_string())
}

/// Whether `destination` starts with a URL scheme. A single letter is a
/// Windows drive, not a scheme.
fn is_url(destination: &str) -> bool {
    let Some((scheme, _)) = destination.split_once(':') else {
        return false;
    };
    scheme.len() > 1
        && scheme.starts_with(|character: char| character.is_ascii_alphabetic())
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// Escapes `specials` with a backslash. A backslash is doubled only where it
/// would otherwise escape what follows it, so text that uses one on its own,
/// such as a Windows path, is written as it reads; [`unescape_markdown`]
/// undoes exactly this.
fn escape_markdown(text: &str, specials: &[char]) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    let mut escaped = String::with_capacity(text.len());
    for (index, &character) in characters.iter().enumerate() {
        if character == '\\' {
            let next = characters.get(index + 1);
            if next.is_none_or(|next| *next == '\\' || specials.contains(next)) {
                escaped.push_str("\\\\");
            } else {
                escaped.push('\\');
            }
        } else {
            if specials.contains(&character) {
                escaped.push('\\');
            }
            escaped.push(character);
        }
    }
    escaped
}

fn unescape_markdown(text: &str, specials: &[char]) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(&next) = characters.peek()
            && (next == '\\' || specials.contains(&next))
        {
            unescaped.push(next);
            characters.next();
        } else {
            unescaped.push(character);
        }
    }
    unescaped
}

/// Undoes the backslash escapes of a link destination, where every escaped
/// punctuation character stands for itself. Unlike the alt text, a destination
/// is never written back with backslashes: what it cannot hold is
/// percent-encoded instead.
fn unescape_destination(destination: &str) -> String {
    let mut unescaped = String::with_capacity(destination.len());
    let mut characters = destination.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(&next) = characters.peek()
            && next.is_ascii_punctuation()
        {
            unescaped.push(next);
            characters.next();
        } else {
            unescaped.push(character);
        }
    }
    unescaped
}

fn decode_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let entity = after.find(';').and_then(|end| {
            let name = &after[..end];
            let character = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| {
                        name.strip_prefix('#')
                            .and_then(|decimal| decimal.parse().ok())
                    })
                    .and_then(char::from_u32),
            }?;
            Some((character, end))
        });
        match entity {
            Some((character, end)) => {
                decoded.push(character);
                rest = &after[end + 1..];
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

/// What the width or height field holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LengthInput {
    Empty,
    /// A number of pixels (`300`) or a percentage (`50%`), as written into the
    /// attribute.
    Valid(String),
    Invalid,
}

/// Reads a width or height. `px` is accepted and dropped, since the attribute
/// counts pixels anyway, and full-width digits typed through a Japanese input
/// method are read as the digits they are.
pub(crate) fn parse_length(input: &str) -> LengthInput {
    let normalized = input
        .chars()
        .map(|character| match character {
            '０'..='９' => {
                char::from_digit(character as u32 - '０' as u32, 10).unwrap_or(character)
            }
            '％' => '%',
            '．' => '.',
            character => character,
        })
        .collect::<String>();
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return LengthInput::Empty;
    }

    let (number, percent) = match trimmed.strip_suffix('%') {
        Some(number) => (number.trim_end(), true),
        None => {
            let lowered = trimmed.to_ascii_lowercase();
            let number = if lowered.ends_with("px") {
                trimmed[..trimmed.len() - 2].trim_end()
            } else {
                trimmed
            };
            (number, false)
        }
    };
    let digits = number.chars().filter(char::is_ascii_digit).count();
    let dots = number.chars().filter(|character| *character == '.').count();
    let valid = digits > 0
        && digits + dots == number.chars().count()
        && dots <= 1
        && !number.starts_with('.')
        && !number.ends_with('.');
    if !valid {
        return LengthInput::Invalid;
    }
    LengthInput::Valid(if percent {
        format!("{number}%")
    } else {
        number.to_string()
    })
}

/// A start tag as read from the buffer.
struct StartTag {
    range: Range<usize>,
    /// Lowercased, for matching.
    name: String,
    /// As written, for closing the tag the way it was opened.
    written_name: String,
    attributes: Vec<TagAttribute>,
    self_closing: bool,
}

struct TagAttribute {
    /// Lowercased, for matching.
    name: String,
    /// The value with its character references decoded.
    value: Option<String>,
    /// The attribute exactly as written.
    source: String,
}

fn skip_whitespace(text: &str, position: usize) -> usize {
    text.get(position..).map_or(position, |rest| {
        position + rest.len() - rest.trim_start().len()
    })
}

fn parse_start_tag(text: &str, start: usize) -> Option<StartTag> {
    let rest = text.get(start..)?.strip_prefix('<')?;
    let name_length = rest
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .unwrap_or(rest.len());
    let written_name = &rest[..name_length];
    if !written_name.starts_with(|character: char| character.is_ascii_alphabetic()) {
        return None;
    }

    let mut position = start + 1 + name_length;
    let mut attributes = Vec::new();
    loop {
        let after_space = skip_whitespace(text, position);
        let had_space = after_space > position;
        position = after_space;
        let rest = text.get(position..)?;
        if rest.starts_with("/>") {
            return Some(StartTag {
                range: start..position + 2,
                name: written_name.to_ascii_lowercase(),
                written_name: written_name.to_string(),
                attributes,
                self_closing: true,
            });
        }
        if rest.starts_with('>') {
            return Some(StartTag {
                range: start..position + 1,
                name: written_name.to_ascii_lowercase(),
                written_name: written_name.to_string(),
                attributes,
                self_closing: false,
            });
        }
        if !had_space {
            return None;
        }

        let attribute_start = position;
        let name_length = rest
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | '>' | '/' | '=' | '<')
            })
            .unwrap_or(rest.len());
        if name_length == 0 {
            return None;
        }
        let name = rest[..name_length].to_ascii_lowercase();
        position += name_length;

        let after_name = skip_whitespace(text, position);
        let mut value = None;
        if text.get(after_name..)?.starts_with('=') {
            let value_start = skip_whitespace(text, after_name + 1);
            let rest = text.get(value_start..)?;
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let length = rest[1..].find(quote)?;
                value = Some(decode_entities(&rest[1..1 + length]));
                position = value_start + length + 2;
            } else {
                let length = rest
                    .find(|character: char| {
                        character.is_whitespace()
                            || matches!(character, '"' | '\'' | '=' | '<' | '>' | '`')
                    })
                    .unwrap_or(rest.len());
                if length == 0 {
                    return None;
                }
                value = Some(decode_entities(&rest[..length]));
                position = value_start + length;
            }
        }
        attributes.push(TagAttribute {
            name,
            value,
            source: text[attribute_start..position].to_string(),
        });
    }
}

/// The end of `</name>` when it starts right at `position`.
fn parse_end_tag(text: &str, position: usize, name: &str) -> Option<usize> {
    let rest = text.get(position..)?.strip_prefix("</")?;
    let written = rest.get(..name.len())?;
    if !written.eq_ignore_ascii_case(name) {
        return None;
    }
    let after_name = position + 2 + name.len();
    let end = skip_whitespace(text, after_name);
    text.get(end..)?.starts_with('>').then_some(end + 1)
}

fn markup_from_image_tag(tag: &StartTag) -> ImageMarkup {
    let mut markup = ImageMarkup {
        self_closing: tag.self_closing,
        ..ImageMarkup::default()
    };
    let mut style = None;
    for attribute in &tag.attributes {
        let kind = match attribute.name.as_str() {
            "src" => Some(KnownAttribute::Source),
            "alt" => Some(KnownAttribute::Alt),
            "width" => Some(KnownAttribute::Width),
            "height" => Some(KnownAttribute::Height),
            "title" => Some(KnownAttribute::Title),
            "style" => Some(KnownAttribute::Style),
            _ => None,
        };
        let value = attribute.value.clone().unwrap_or_default();
        // A second spelling of an attribute is ignored by browsers and kept as
        // it is here; so is a width or height the fields cannot hold.
        let kind = kind.filter(|kind| {
            !markup.image_slots.contains(&AttributeSlot::Known(*kind))
                && match kind {
                    KnownAttribute::Width | KnownAttribute::Height => {
                        matches!(parse_length(&value), LengthInput::Valid(_))
                    }
                    _ => true,
                }
        });
        let Some(kind) = kind else {
            markup.image_slots.push(AttributeSlot::Verbatim {
                name: attribute.name.clone(),
                source: attribute.source.clone(),
            });
            continue;
        };
        match kind {
            KnownAttribute::Source => {
                markup.destination = decode_destination(&value);
                markup.written_destination = Some(value);
            }
            KnownAttribute::Alt => markup.alt = value,
            KnownAttribute::Width => markup.width = valid_length(&value),
            KnownAttribute::Height => markup.height = valid_length(&value),
            KnownAttribute::Title => markup.title = Some(value),
            KnownAttribute::Style => style = Some(value),
            KnownAttribute::Align => {}
        }
        markup.image_slots.push(AttributeSlot::Known(kind));
    }

    if let Some(style) = style {
        let mut kept = Vec::new();
        for declaration in style.split(';') {
            let declaration = declaration.trim();
            if declaration.is_empty() {
                continue;
            }
            let taken = declaration
                .split_once(':')
                .is_some_and(|(property, value)| {
                    let field = match property.trim().to_ascii_lowercase().as_str() {
                        "width" => &mut markup.width,
                        "height" => &mut markup.height,
                        _ => return false,
                    };
                    if field.is_some() {
                        return false;
                    }
                    *field = valid_length(value);
                    field.is_some()
                });
            if !taken {
                kept.push(declaration);
            }
        }
        markup.style = (!kept.is_empty()).then(|| kept.join("; "));
    }
    markup
}

fn valid_length(value: &str) -> Option<String> {
    match parse_length(value) {
        LengthInput::Valid(length) => Some(length),
        LengthInput::Empty | LengthInput::Invalid => None,
    }
}

/// Whether the image at `image` is the one `selection` points at: the cursor
/// is on it, the selection lies within it, or the selection holds it and
/// nothing but whitespace besides.
fn covers(image: &Range<usize>, selection: &Range<usize>, text: &str) -> bool {
    if selection.is_empty() {
        return image.start <= selection.start && selection.start <= image.end;
    }
    if image.start <= selection.start && selection.end <= image.end {
        return true;
    }
    selection.start <= image.start
        && image.end <= selection.end
        && text
            .get(selection.start..image.start)
            .is_some_and(|before| before.trim().is_empty())
        && text
            .get(image.end..selection.end)
            .is_some_and(|after| after.trim().is_empty())
}

/// The image `selection` points at in `text`, with the range it spans.
pub(crate) fn image_at(text: &str, selection: Range<usize>) -> Option<(Range<usize>, ImageMarkup)> {
    container_image_at(text, &selection)
        .or_else(|| html_image_at(text, &selection))
        .or_else(|| markdown_image_at(text, &selection))
}

/// What a line starts with before its content: indentation, the `>` of the
/// block quotes it is in and the marker of a list item it opens.
#[derive(Debug, PartialEq, Eq)]
struct LinePrefix {
    /// Bytes of the line it takes.
    length: usize,
    /// What the lines after it start with to stay in the same block quotes and
    /// list items: each `>` kept, each list marker turned into its width in
    /// spaces.
    continuation: String,
    quote_depth: usize,
    /// The column of the list marker the line opens, if it opens one.
    list_marker_column: Option<usize>,
}

fn line_prefix(line: &str) -> LinePrefix {
    let mut prefix = LinePrefix {
        length: 0,
        continuation: String::new(),
        quote_depth: 0,
        list_marker_column: None,
    };
    while let Some(rest) = line.get(prefix.length..) {
        let content = rest.trim_start_matches([' ', '\t']);
        let indentation = rest.len() - content.len();
        prefix.continuation.push_str(&rest[..indentation]);
        prefix.length += indentation;
        if let Some(after) = content.strip_prefix('>') {
            prefix.quote_depth += 1;
            prefix.continuation.push('>');
            prefix.length += 1;
            if after.starts_with(' ') {
                prefix.continuation.push(' ');
                prefix.length += 1;
            }
            continue;
        }
        let Some(marker_length) = list_marker_length(content) else {
            break;
        };
        let after = content.get(marker_length..).unwrap_or_default();
        let spaces = after.len() - after.trim_start_matches(' ').len();
        // The content of an empty item would start one space after its marker,
        // and content set off by five spaces or more is indented code, whose
        // indentation is its own.
        let (taken, width) = if after.trim().is_empty() {
            (spaces, 1)
        } else if spaces > 4 {
            (1, 1)
        } else {
            (spaces, spaces)
        };
        prefix.list_marker_column = Some(prefix.continuation.chars().count());
        prefix
            .continuation
            .extend(std::iter::repeat_n(' ', marker_length + width));
        prefix.length += marker_length + taken;
    }
    prefix
}

/// The length of the list marker `text` starts with: `-`, `*` or `+`, or up to
/// nine digits and `.` or `)`, followed by a space, a tab or nothing.
fn list_marker_length(text: &str) -> Option<usize> {
    let digits = text.len()
        - text
            .trim_start_matches(|character: char| character.is_ascii_digit())
            .len();
    let length = if digits == 0 {
        if !text.starts_with(['-', '*', '+']) {
            return None;
        }
        1
    } else if digits <= 9 && text.get(digits..)?.starts_with(['.', ')']) {
        digits + 1
    } else {
        return None;
    };
    let after = text.get(length..)?;
    (after.is_empty() || after.starts_with([' ', '\t', '\n', '\r'])).then_some(length)
}

/// Whether a line holds nothing but what keeps it in its block quotes and list
/// items, which makes it a blank line inside them.
fn is_blank_within_containers(line: &str) -> bool {
    line.get(line_prefix(line).length..)
        .is_none_or(|content| content.trim().is_empty())
}

/// A `<div align>` or `<p align>` that starts the content of a line and holds
/// nothing but one `<img>`.
fn container_image_at(text: &str, selection: &Range<usize>) -> Option<(Range<usize>, ImageMarkup)> {
    let mut line_start = 0;
    loop {
        let line_end = text
            .get(line_start..)?
            .find('\n')
            .map_or(text.len(), |length| line_start + length);
        let prefix = line_prefix(text.get(line_start..line_end)?);
        if let Some(open) = parse_start_tag(text, line_start + prefix.length)
            && matches!(open.name.as_str(), "div" | "p")
            && !open.self_closing
            && let Some((range, markup)) = container_body(text, &open, prefix.quote_depth)
            && covers(&range, selection, text)
        {
            return Some((range, markup));
        }
        line_start += text.get(line_start..)?.find('\n')? + 1;
    }
}

/// Skips the whitespace between two tags of one HTML block, along with the
/// `>` that keep each new line in the block quotes the block is in. `None`
/// when a blank line comes first, since that ends the block.
fn skip_block_whitespace(text: &str, position: usize, quote_depth: usize) -> Option<usize> {
    let mut position = position;
    loop {
        let rest = text.get(position..)?;
        position += rest.len() - rest.trim_start_matches([' ', '\t', '\r']).len();
        if !text.get(position..)?.starts_with('\n') {
            return Some(position);
        }
        position += 1;
        for _ in 0..quote_depth {
            let rest = text.get(position..)?;
            let indentation = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            // An HTML block does not continue lazily: a line without its `>`
            // has left the quote.
            if !rest.get(indentation..)?.starts_with('>') {
                return None;
            }
            position += indentation + 1;
            if text.get(position..)?.starts_with(' ') {
                position += 1;
            }
        }
        let line = text.get(position..)?;
        let line = line.split('\n').next().unwrap_or(line);
        if line.trim().is_empty() {
            return None;
        }
    }
}

fn container_body(
    text: &str,
    open: &StartTag,
    quote_depth: usize,
) -> Option<(Range<usize>, ImageMarkup)> {
    let image_start = skip_block_whitespace(text, open.range.end, quote_depth)?;
    let image = parse_start_tag(text, image_start)?;
    if image.name != "img" {
        return None;
    }
    let close_start = skip_block_whitespace(text, image.range.end, quote_depth)?;
    let close_end = parse_end_tag(text, close_start, &open.name)?;

    let mut markup = markup_from_image_tag(&image);
    let mut slots = Vec::new();
    for attribute in &open.attributes {
        let alignment = attribute
            .value
            .as_deref()
            .and_then(ImageAlignment::from_attribute);
        match alignment {
            Some(alignment) if attribute.name == "align" && markup.alignment.is_none() => {
                markup.alignment = Some(alignment);
                slots.push(AttributeSlot::Known(KnownAttribute::Align));
            }
            _ => slots.push(AttributeSlot::Verbatim {
                name: attribute.name.clone(),
                source: attribute.source.clone(),
            }),
        }
    }
    markup.container = Some(ImageContainer {
        tag: open.written_name.clone(),
        slots,
    });
    Some((open.range.start..close_end, markup))
}

fn html_image_at(text: &str, selection: &Range<usize>) -> Option<(Range<usize>, ImageMarkup)> {
    text.match_indices('<').find_map(|(start, _)| {
        let tag = parse_start_tag(text, start)?;
        (tag.name == "img" && covers(&tag.range, selection, text))
            .then(|| (tag.range.clone(), markup_from_image_tag(&tag)))
    })
}

fn markdown_image_at(text: &str, selection: &Range<usize>) -> Option<(Range<usize>, ImageMarkup)> {
    text.match_indices("![").find_map(|(start, _)| {
        let backslashes = text[..start]
            .chars()
            .rev()
            .take_while(|character| *character == '\\')
            .count();
        if backslashes % 2 == 1 {
            return None;
        }
        let (end, markup) = parse_markdown_image(text, start)?;
        let range = start..end;
        covers(&range, selection, text).then_some((range, markup))
    })
}

/// Reads `![alt](destination "title")` starting at `start`, returning where it
/// ends.
fn parse_markdown_image(text: &str, start: usize) -> Option<(usize, ImageMarkup)> {
    let alt_start = start + 2;
    let mut depth = 1;
    let mut characters = text.get(alt_start..)?.char_indices();
    let alt_end = loop {
        let (offset, character) = characters.next()?;
        match character {
            '\\' => {
                characters.next();
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break alt_start + offset;
                }
            }
            _ => {}
        }
    };
    let alt = &text[alt_start..alt_end];
    let mut position = alt_end + 1;
    if !text.get(position..)?.starts_with('(') {
        return None;
    }
    position = skip_whitespace(text, position + 1);

    let rest = text.get(position..)?;
    let destination = if let Some(inner) = rest.strip_prefix('<') {
        let length = inner.find(['>', '\n'])?;
        if !inner[length..].starts_with('>') {
            return None;
        }
        position += length + 2;
        &inner[..length]
    } else {
        let mut parentheses = 0usize;
        let mut length = rest.len();
        let mut characters = rest.char_indices();
        while let Some((offset, character)) = characters.next() {
            match character {
                '\\' => {
                    characters.next();
                }
                '(' => parentheses += 1,
                ')' if parentheses == 0 => {
                    length = offset;
                    break;
                }
                ')' => parentheses -= 1,
                // Only ASCII spaces end a destination, so an ideographic space
                // in a Japanese file name is part of it, as the preview reads it.
                character if character.is_ascii_whitespace() || character.is_ascii_control() => {
                    length = offset;
                    break;
                }
                _ => {}
            }
        }
        position += length;
        &rest[..length]
    };

    let after_destination = skip_whitespace(text, position);
    let mut title = None;
    let rest = text.get(after_destination..)?;
    if let Some(delimiter) = rest
        .chars()
        .next()
        .filter(|character| after_destination > position && matches!(character, '"' | '\'' | '('))
    {
        let closing = if delimiter == '(' { ')' } else { delimiter };
        let mut characters = rest.char_indices().skip(1);
        let length = loop {
            let (offset, character) = characters.next()?;
            if character == '\\' {
                characters.next();
            } else if character == closing {
                break offset;
            }
        };
        title = Some(unescape_markdown(&rest[1..length], &[delimiter, closing]));
        position = skip_whitespace(text, after_destination + length + 1);
    } else {
        position = after_destination;
    }
    if !text.get(position..)?.starts_with(')') {
        return None;
    }

    let written = decode_entities(&unescape_destination(destination));
    let markup = ImageMarkup {
        destination: decode_destination(&written),
        written_destination: Some(written),
        alt: unescape_markdown(alt, &['[', ']']),
        title,
        ..ImageMarkup::default()
    };
    Some((position + 1, markup))
}

/// Where the Markdown file sits, which is what paths are written relative to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocumentLocation {
    /// The folder the document is in; `None` for a buffer never saved.
    pub directory: Option<String>,
    /// The root of the worktree the document belongs to. A file outside it
    /// would not travel with the document, which is when copying is offered.
    pub root: Option<String>,
    pub path_style: PathStyle,
    /// Whether the files are on this machine, which the file picker and the
    /// copy need.
    pub is_local: bool,
}

fn document_location(
    editor: &Entity<Editor>,
    project: &Entity<Project>,
    cx: &App,
) -> DocumentLocation {
    let project = project.read(cx);
    let path_style = project.path_style(cx);
    let is_local = project.is_local();
    let unsaved = DocumentLocation {
        directory: None,
        root: None,
        path_style,
        is_local,
    };
    let Some(file) = editor
        .read(cx)
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).file().cloned())
    else {
        return unsaved;
    };
    let Some(worktree) = project.worktree_for_id(file.worktree_id(cx), cx) else {
        return unsaved;
    };
    let root = worktree.read(cx).abs_path().to_string_lossy().into_owned();
    let relative = file.path();
    if relative.is_empty() {
        // A file opened on its own is a worktree by itself; its folder stands
        // in for the root so its neighbours do not count as outside.
        let directory = parent_directory(&root, path_style);
        return DocumentLocation {
            root: directory.clone(),
            directory,
            path_style,
            is_local,
        };
    }
    let Some(file_path) = path_style.join(&root, relative.display(path_style).as_ref()) else {
        return unsaved;
    };
    DocumentLocation {
        directory: parent_directory(&file_path, path_style),
        root: Some(root),
        path_style,
        is_local,
    }
}

fn parent_directory(path: &str, style: PathStyle) -> Option<String> {
    let separator = path.rfind(style.separators_ch())?;
    let parent = &path[..separator];
    // The parent of `/a.md` is `/`, and the parent of `C:\a.md` is `C:\`.
    if parent.is_empty() || (style.is_windows() && parent.ends_with(':')) {
        Some(path[..separator + 1].to_string())
    } else {
        Some(parent.to_string())
    }
}

/// An absolute path split into what it is rooted at and its components, with
/// `.` and `..` resolved. `None` for a path that is not absolute.
fn split_absolute(path: &str, style: PathStyle) -> Option<(String, Vec<String>)> {
    let (root, rest) = match style {
        PathStyle::Windows => {
            if let Some(unc) = path
                .strip_prefix("\\\\")
                .or_else(|| path.strip_prefix("//"))
            {
                let mut parts = unc.splitn(3, ['\\', '/']);
                let server = parts.next().filter(|server| !server.is_empty())?;
                let share = parts.next().filter(|share| !share.is_empty())?;
                (format!("//{server}/{share}"), parts.next().unwrap_or(""))
            } else {
                let mut characters = path.chars();
                let drive = characters.next().filter(char::is_ascii_alphabetic)?;
                if characters.next() != Some(':') {
                    return None;
                }
                let rest = path.get(2..)?;
                // `C:foo` is relative to that drive's current folder.
                if !rest.starts_with(['\\', '/']) {
                    return None;
                }
                (format!("{}:", drive.to_ascii_uppercase()), rest)
            }
        }
        PathStyle::Unix => ("/".to_string(), path.strip_prefix('/')?),
    };
    let mut components = Vec::new();
    for component in rest.split(style.separators_ch()) {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component.to_string()),
        }
    }
    Some((root, components))
}

fn same_component(left: &str, right: &str, style: PathStyle) -> bool {
    left == right || (style.is_windows() && left.to_lowercase() == right.to_lowercase())
}

/// The path from `from_directory` to `to_file`, `/`-separated and without a
/// leading `./`. `None` when there is none, as between two Windows drives.
pub(crate) fn relative_path(
    from_directory: &str,
    to_file: &str,
    style: PathStyle,
) -> Option<String> {
    let (from_root, from) = split_absolute(from_directory, style)?;
    let (to_root, to) = split_absolute(to_file, style)?;
    if !same_component(&from_root, &to_root, style) {
        return None;
    }
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| same_component(left, right, style))
        .count();
    if common == to.len() {
        return None;
    }
    let mut parts = vec![".."; from.len() - common];
    parts.extend(to[common..].iter().map(String::as_str));
    Some(parts.join("/"))
}

/// How many folders apart `to_file` is from `from_directory`, counting the
/// steps up and the steps down.
fn folder_distance(from_directory: &str, to_file: &str, style: PathStyle) -> Option<usize> {
    let (from_root, from) = split_absolute(from_directory, style)?;
    let (to_root, mut to) = split_absolute(to_file, style)?;
    if !same_component(&from_root, &to_root, style) {
        return None;
    }
    to.pop();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| same_component(left, right, style))
        .count();
    Some(from.len() - common + to.len() - common)
}

fn is_within(directory: &str, path: &str, style: PathStyle) -> bool {
    let (Some((directory_root, directory)), Some((path_root, path))) = (
        split_absolute(directory, style),
        split_absolute(path, style),
    ) else {
        return false;
    };
    same_component(&directory_root, &path_root, style)
        && path.len() > directory.len()
        && directory
            .iter()
            .zip(&path)
            .all(|(left, right)| same_component(left, right, style))
}

fn join_path(directory: &str, relative: &str, style: PathStyle) -> Option<String> {
    let joined = style.join(directory, relative)?;
    Some(style.normalize(&joined))
}

/// What the path field points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedSource {
    Empty,
    Url(String),
    File(ResolvedFile),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedFile {
    /// The file on disk, when the path can be anchored anywhere.
    pub absolute: Option<String>,
    /// The path from the document's folder, which is what gets written.
    /// `None` when there is no such path.
    pub relative: Option<String>,
    /// Whether the file lies inside the worktree that holds the document.
    pub inside_root: bool,
}

impl ResolvedFile {
    /// The destination written when the file is referred to where it is: the
    /// relative path, or failing that the absolute one with `/` separators.
    fn destination(&self) -> Option<String> {
        self.relative.clone().or_else(|| {
            self.absolute
                .as_ref()
                .map(|absolute| absolute.replace('\\', "/"))
        })
    }
}

pub(crate) fn resolve_source(input: &str, location: &DocumentLocation) -> ResolvedSource {
    let trimmed = input.trim();
    // Windows' "Copy as path" puts quotes around what it copies.
    let input = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim();
    if input.is_empty() {
        return ResolvedSource::Empty;
    }
    if is_url(input) {
        return ResolvedSource::Url(input.to_string());
    }

    let style = location.path_style;
    let directory = location.directory.as_deref();
    let (absolute, relative) = if style.is_absolute(input) {
        if split_absolute(input, style).is_none() {
            // `/images/a.png` on Windows names no drive, so it cannot be looked
            // up; it is written as it is, which the preview and GitHub both
            // read from the root of the project.
            return ResolvedSource::File(ResolvedFile {
                absolute: None,
                relative: Some(input.replace('\\', "/")),
                inside_root: false,
            });
        }
        let relative = directory.and_then(|directory| relative_path(directory, input, style));
        (Some(style.normalize(input)), relative)
    } else {
        let mut relative = if style.is_windows() {
            input.replace('\\', "/")
        } else {
            input.to_string()
        };
        while let Some(rest) = relative.strip_prefix("./") {
            relative = rest.to_string();
        }
        let absolute = directory.and_then(|directory| join_path(directory, &relative, style));
        (absolute, Some(relative))
    };
    let inside_root = match (&location.root, &absolute) {
        (Some(root), Some(absolute)) => is_within(root, absolute, style),
        _ => false,
    };
    ResolvedSource::File(ResolvedFile {
        absolute,
        relative,
        inside_root,
    })
}

/// The file name the alt text starts out as: the last component without its
/// extension.
fn default_alt(input: &str) -> String {
    let input = input.trim().trim_matches('"');
    let without_query = if is_url(input) {
        input.split(['?', '#']).next().unwrap_or(input)
    } else {
        input
    };
    let file_name = without_query
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(without_query);
    match file_name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => file_name.to_string(),
    }
}

/// A name in `directory` that nothing occupies yet: `file_name` itself, or
/// with `-1`, `-2`, … before its extension.
pub(crate) async fn unused_file_name(
    fs: &dyn Fs,
    directory: &Path,
    file_name: &str,
) -> Result<String> {
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (file_name, String::new()),
    };
    for attempt in 0..10_000 {
        let candidate = if attempt == 0 {
            file_name.to_string()
        } else {
            format!("{stem}-{attempt}{extension}")
        };
        if fs.metadata(&directory.join(&candidate)).await?.is_none() {
            return Ok(candidate);
        }
    }
    Err(anyhow!("コピー先に使える名前が見つかりません: {file_name}"))
}

/// Copies `source` into the `images` folder next to the document and returns
/// the name it was given there.
pub(crate) async fn copy_into_images(
    fs: Arc<dyn Fs>,
    source: PathBuf,
    document_directory: PathBuf,
) -> Result<String> {
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("ファイル名を読み取れません: {}", source.display()))?;
    let images = document_directory.join("images");
    fs.create_dir(&images).await?;
    let name = unused_file_name(fs.as_ref(), &images, file_name).await?;
    fs.copy_file(&source, &images.join(&name), CopyOptions::default())
        .await?;
    Ok(name)
}

/// Where the dialog writes, and what was there when it opened.
pub(crate) struct ImageTarget {
    range: Range<Anchor>,
    /// Checked again before writing, so an edit that landed underneath the
    /// dialog is noticed instead of overwritten.
    original_text: String,
    existing: Option<ImageMarkup>,
    /// Selected text that is not an image, which the alt text starts out as.
    selected_text: Option<String>,
    /// Why an HTML block cannot be written here, where splitting the line
    /// would break what the image sits in.
    block_refusal: Option<&'static str>,
    /// Whether the image is, or is to be, inside an HTML block, where it has to
    /// stay an `<img>` and a block of its own cannot be set apart by blank
    /// lines.
    inside_html: bool,
    /// Whether a new image lands in the middle of a line, where an aligned one
    /// goes after the paragraph instead of splitting the line and whatever
    /// formatting runs across the cursor.
    after_paragraph: bool,
}

/// How an HTML block ends: at the line that holds one of the markers, or
/// before the next blank line.
enum HtmlBlockEnd {
    Markers(&'static [&'static str]),
    BlankLine,
}

/// Tags that start an HTML block wherever they begin a line, even in the
/// middle of a paragraph.
const HTML_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// Elements that have no end tag, and so never hold what follows them.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// Whether the content of a line starts an HTML block, and how that block
/// ends. A lone tag of any other name starts one only where it would not
/// continue a paragraph.
fn html_block_start(content: &str, continues_paragraph: bool) -> Option<HtmlBlockEnd> {
    let content = content.trim_start_matches([' ', '\t']);
    if !content.starts_with('<') {
        return None;
    }
    let lowered = content.to_ascii_lowercase();
    let names_end = |after: &str| after.is_empty() || after.starts_with([' ', '\t', '>']);
    for raw in ["script", "pre", "style", "textarea"] {
        if lowered
            .strip_prefix('<')
            .and_then(|rest| rest.strip_prefix(raw))
            .is_some_and(names_end)
        {
            return Some(HtmlBlockEnd::Markers(&[
                "</script>",
                "</pre>",
                "</style>",
                "</textarea>",
            ]));
        }
    }
    if lowered.starts_with("<!--") {
        return Some(HtmlBlockEnd::Markers(&["-->"]));
    }
    if lowered.starts_with("<?") {
        return Some(HtmlBlockEnd::Markers(&["?>"]));
    }
    if lowered.starts_with("<![cdata[") {
        return Some(HtmlBlockEnd::Markers(&["]]>"]));
    }
    if lowered
        .strip_prefix("<!")
        .is_some_and(|rest| rest.starts_with(|character: char| character.is_ascii_alphabetic()))
    {
        return Some(HtmlBlockEnd::Markers(&[">"]));
    }

    let name_start = if lowered.starts_with("</") { 2 } else { 1 };
    let rest = lowered.get(name_start..).unwrap_or_default();
    let name_length = rest
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    let name = rest.get(..name_length).unwrap_or_default();
    let after = rest.get(name_length..).unwrap_or_default();
    if HTML_BLOCK_TAGS.contains(&name) && (names_end(after) || after.starts_with("/>")) {
        return Some(HtmlBlockEnd::BlankLine);
    }
    if continues_paragraph || name.is_empty() {
        return None;
    }
    let tag_end = if name_start == 2 {
        let after_name = after.trim_start_matches([' ', '\t']);
        after_name
            .starts_with('>')
            .then(|| content.len() - after_name.len() + 1)
    } else {
        parse_start_tag(content, 0).map(|tag| tag.range.end)
    };
    tag_end
        .and_then(|end| content.get(end..))
        .is_some_and(|rest| rest.trim().is_empty())
        .then_some(HtmlBlockEnd::BlankLine)
}

/// The HTML blocks of `block`, a run of lines with no blank line between
/// them, as ranges from where each starts to the end of its last line.
fn html_blocks(block: &str) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, HtmlBlockEnd)> = None;
    let mut continues_paragraph = false;
    let mut line_start = 0;
    for line in block.split('\n') {
        let line_end = line_start + line.len();
        let prefix = line_prefix(line);
        let content = line.get(prefix.length..).unwrap_or_default();
        if content.trim().is_empty() {
            if let Some((start, HtmlBlockEnd::BlankLine)) = open {
                blocks.push(start..line_start.saturating_sub(1));
                open = None;
            }
            continues_paragraph = false;
            line_start = line_end + 1;
            continue;
        }
        if open.is_none() {
            let in_paragraph = continues_paragraph && prefix.list_marker_column.is_none();
            open = html_block_start(content, in_paragraph)
                .map(|end| (line_start + prefix.length, end));
        }
        match &open {
            Some((start, HtmlBlockEnd::Markers(markers))) => {
                let lowered = content.to_ascii_lowercase();
                if markers.iter().any(|marker| lowered.contains(marker)) {
                    blocks.push(*start..line_end);
                    open = None;
                }
                continues_paragraph = false;
            }
            Some(_) => continues_paragraph = false,
            None => continues_paragraph = !is_atx_heading(content),
        }
        line_start = line_end + 1;
    }
    if let Some((start, _)) = open {
        blocks.push(start..block.len());
    }
    blocks
}

/// Whether `target` lies inside one of `html_blocks` without being all of it:
/// an `<img>` alone on its line is an HTML block of its own, which a block in
/// its place can replace.
fn is_inside_html(html_blocks: &[Range<usize>], block: &str, target: &Range<usize>) -> bool {
    let is_blank = |text: Option<&str>| {
        text.is_some_and(|text| {
            text.trim_matches(|character: char| character.is_whitespace() || character == '>')
                .is_empty()
        })
    };
    html_blocks.iter().any(|html| {
        html.start <= target.start
            && target.start <= html.end
            && !(is_blank(block.get(html.start..target.start))
                && is_blank(block.get(target.end.min(html.end)..html.end)))
    })
}

/// Whether `text` leaves an element open, such as the `<a>` of
/// `<a href="…"><img …></a>`, which an HTML block cannot be put inside.
fn leaves_an_element_open(text: &str) -> bool {
    let mut open = Vec::new();
    for (start, _) in text.match_indices('<') {
        if let Some(rest) = text.get(start..).and_then(|rest| rest.strip_prefix("</")) {
            let name = rest
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            if let Some(position) = open.iter().rposition(|open_name| *open_name == name) {
                open.truncate(position);
            }
        } else if let Some(tag) = parse_start_tag(text, start)
            && !tag.self_closing
            && !VOID_ELEMENTS.contains(&tag.name.as_str())
        {
            open.push(tag.name);
        }
    }
    !open.is_empty()
}

/// A line with its indentation and block quote markers taken off, which is
/// where headings, fences and rules are told apart. List markers stay, since
/// `- - -` is a rule rather than three nested items.
fn after_quotes(line: &str) -> &str {
    line.trim_start_matches(|character: char| matches!(character, ' ' | '\t' | '>'))
}

fn is_atx_heading(content: &str) -> bool {
    let content = content.trim_start_matches([' ', '\t']);
    let hashes = content.len() - content.trim_start_matches('#').len();
    (1..=6).contains(&hashes)
        && content
            .get(hashes..)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

fn is_setext_underline(content: &str) -> bool {
    let content = content.trim();
    !content.is_empty()
        && (content.chars().all(|character| character == '=')
            || content.chars().all(|character| character == '-'))
}

fn is_thematic_break(content: &str) -> bool {
    let marks = content
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<Vec<_>>();
    marks.len() >= 3
        && marks.first().is_some_and(|first| {
            matches!(first, '*' | '-' | '_') && marks.iter().all(|mark| mark == first)
        })
}

fn is_fence(content: &str) -> bool {
    let content = content.trim_start_matches([' ', '\t']);
    content.starts_with("```") || content.starts_with("~~~")
}

/// The row a paragraph ends on, given one of its rows: a heading is its own
/// line, a setext heading takes in its underline, and anything that starts a
/// block of its own ends the paragraph before it.
fn paragraph_end_row(snapshot: &BufferSnapshot, row: u32) -> u32 {
    let line = line_text(snapshot, row);
    if is_atx_heading(after_quotes(&line)) {
        return row;
    }
    let quote_depth = line_prefix(&line).quote_depth;
    let max_row = snapshot.max_point().row;
    let mut end = row;
    while end < max_row {
        let next = line_text(snapshot, end + 1);
        let raw = after_quotes(&next);
        if is_setext_underline(raw) && !is_blank_within_containers(&next) {
            return end + 1;
        }
        let prefix = line_prefix(&next);
        let content = next.get(prefix.length..).unwrap_or_default();
        if is_blank_within_containers(&next)
            || prefix.list_marker_column.is_some()
            || prefix.quote_depth > quote_depth
            || is_atx_heading(raw)
            || is_fence(raw)
            || is_thematic_break(raw)
            || html_block_start(content, true).is_some()
        {
            break;
        }
        end += 1;
    }
    end
}

/// The row a paragraph starts on, given one of its rows, whose line prefix is
/// what the lines of a block after the paragraph continue with.
fn paragraph_start_row(snapshot: &BufferSnapshot, row: u32) -> u32 {
    let mut start = row;
    while start > 0 {
        let line = line_text(snapshot, start);
        if line_prefix(&line).list_marker_column.is_some() || is_atx_heading(after_quotes(&line)) {
            break;
        }
        let previous = line_text(snapshot, start - 1);
        let raw = after_quotes(&previous);
        if is_blank_within_containers(&previous)
            || is_atx_heading(raw)
            || is_fence(raw)
            || is_thematic_break(raw)
            || is_setext_underline(raw)
        {
            break;
        }
        start -= 1;
    }
    start
}

fn line_text(snapshot: &BufferSnapshot, row: u32) -> String {
    let start = snapshot.point_to_offset(language::Point::new(row, 0));
    let end = snapshot.point_to_offset(language::Point::new(row, snapshot.line_len(row)));
    snapshot.text_for_range(start..end).collect()
}

/// The text of `range`'s lines before and after it.
fn text_around(snapshot: &BufferSnapshot, range: &Range<usize>) -> (String, String) {
    let start_row = snapshot.offset_to_point(range.start).row;
    let end_row = snapshot.offset_to_point(range.end).row;
    let line_start = snapshot.point_to_offset(language::Point::new(start_row, 0));
    let line_end =
        snapshot.point_to_offset(language::Point::new(end_row, snapshot.line_len(end_row)));
    (
        snapshot.text_for_range(line_start..range.start).collect(),
        snapshot.text_for_range(range.end..line_end).collect(),
    )
}

fn is_table_delimiter_row(line: &str) -> bool {
    let content = line
        .get(line_prefix(line).length..)
        .unwrap_or_default()
        .trim();
    content.contains('|')
        && content.contains('-')
        && content
            .chars()
            .all(|character| matches!(character, '|' | '-' | ':' | ' ' | '\t'))
}

/// Why an aligned image cannot be written at `range`. `splits_the_line` is
/// whether the block would go right there rather than after the paragraph,
/// which is when what the line holds around it matters.
fn block_refusal(
    snapshot: &BufferSnapshot,
    range: &Range<usize>,
    block: &str,
    inside_html: bool,
    splits_the_line: bool,
) -> Option<&'static str> {
    const HTML_REFUSAL: &str =
        "HTML の中の画像には配置を指定できません。配置を「指定なし」にしてください。";
    if inside_html {
        return Some(HTML_REFUSAL);
    }
    let (before, after) = text_around(snapshot, range);
    if splits_the_line {
        if before.trim_end().ends_with('[') || after.trim_start().starts_with(']') {
            return Some(
                "リンクの中の画像には配置を指定できません。配置を「指定なし」にしてください。",
            );
        }
        if leaves_an_element_open(&before) {
            return Some(HTML_REFUSAL);
        }
    }
    if (before.contains('|') || after.contains('|')) && block.lines().any(is_table_delimiter_row) {
        return Some("表の中の画像には配置を指定できません。配置を「指定なし」にしてください。");
    }
    None
}

/// The rows of non-blank lines around the selection, as text with the offset
/// it starts at. An image, HTML block included, never spans a blank line.
fn surrounding_block(snapshot: &BufferSnapshot, selection: &Range<usize>) -> (usize, String) {
    let start_row = snapshot.offset_to_point(selection.start).row;
    let end_row = snapshot.offset_to_point(selection.end).row;
    let mut first_row = start_row;
    while first_row > 0 && !snapshot.is_line_blank(first_row - 1) {
        first_row -= 1;
    }
    let max_row = snapshot.max_point().row;
    let mut last_row = end_row;
    while last_row < max_row && !snapshot.is_line_blank(last_row + 1) {
        last_row += 1;
    }
    let start = snapshot.point_to_offset(language::Point::new(first_row, 0));
    let end = snapshot.point_to_offset(language::Point::new(last_row, snapshot.line_len(last_row)));
    (start, snapshot.text_for_range(start..end).collect())
}

fn image_target(editor: &mut Editor, cx: &mut Context<Editor>) -> Option<ImageTarget> {
    let selection = editor
        .selections
        .newest::<MultiBufferOffset>(&editor.display_snapshot(cx))
        .range();
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let snapshot = buffer.read(cx).snapshot();
    let selection = selection.start.0..selection.end.0;

    let (block_start, block) = surrounding_block(&snapshot, &selection);
    let in_block =
        selection.start.saturating_sub(block_start)..selection.end.saturating_sub(block_start);
    let html_blocks = html_blocks(&block);
    if let Some((range_in_block, markup)) = image_at(&block, in_block.clone()) {
        let range = block_start + range_in_block.start..block_start + range_in_block.end;
        let inside_html = is_inside_html(&html_blocks, &block, &range_in_block);
        // A container is a block already, and is replaced as one.
        let block_refusal = markup
            .container
            .is_none()
            .then(|| block_refusal(&snapshot, &range, &block, inside_html, true))
            .flatten();
        return Some(ImageTarget {
            original_text: snapshot.text_for_range(range.clone()).collect(),
            range: snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
            existing: Some(markup),
            selected_text: None,
            block_refusal,
            inside_html,
            after_paragraph: false,
        });
    }

    let original_text: String = snapshot.text_for_range(selection.clone()).collect();
    let selected_text = original_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let inside_html = is_inside_html(&html_blocks, &block, &in_block);
    let (before, after) = text_around(&snapshot, &selection);
    let after_paragraph = before
        .get(line_prefix(&before).length..)
        .is_some_and(|content| !content.trim().is_empty())
        && !after.trim().is_empty();
    Some(ImageTarget {
        range: snapshot.anchor_before(selection.start)..snapshot.anchor_after(selection.end),
        original_text,
        existing: None,
        selected_text: (!selected_text.is_empty()).then_some(selected_text),
        block_refusal: block_refusal(&snapshot, &selection, &block, inside_html, !after_paragraph),
        inside_html,
        after_paragraph,
    })
}

/// Newlines that put an HTML block on lines of its own with a blank line on
/// either side: before it, so it does not continue the paragraph it was typed
/// into, and after it, so the next paragraph is not swallowed into it. Inside
/// a list item or a block quote every new line starts with what keeps it
/// there, or the block would fall out and split the list or the quote.
fn block_padding(
    snapshot: &BufferSnapshot,
    range: &Range<usize>,
    prefix: &LinePrefix,
) -> (String, String) {
    let continuation = prefix.continuation.as_str();
    let blank = continuation.trim_end();
    let start = snapshot.offset_to_point(range.start);
    let end = snapshot.offset_to_point(range.end);
    let (before, after) = text_around(snapshot, range);
    // Read from the line itself: after a paragraph, `prefix` belongs to the
    // paragraph's first line rather than to the line the block follows.
    let before_prefix = line_prefix(&before);
    let content_before = before.get(before_prefix.length..).unwrap_or_default();

    let leading = if !content_before.trim().is_empty() {
        format!("\n{blank}\n{continuation}")
    } else if before_prefix.list_marker_column.is_some() {
        // An HTML block may start a list item, which leaves the item and the
        // list as they were; only a marker typed without its space needs one.
        if before.ends_with([' ', '\t']) {
            String::new()
        } else {
            " ".to_string()
        }
    } else if start.row > 0 && !is_blank_within_containers(&line_text(snapshot, start.row - 1)) {
        format!("\n{continuation}")
    } else {
        String::new()
    };

    let trailing = if !after.trim().is_empty() {
        format!("\n{blank}\n{continuation}")
    } else if end.row < snapshot.max_point().row {
        let next = line_text(snapshot, end.row + 1);
        // A list item that starts left of this content ends the item, and the
        // block with it, without the blank line that would loosen a tight
        // list. Anything else would be swallowed into the block.
        let ends_the_item = line_prefix(&next)
            .list_marker_column
            .is_some_and(|column| column < continuation.chars().count());
        if is_blank_within_containers(&next) || ends_the_item {
            String::new()
        } else {
            format!("\n{blank}")
        }
    } else {
        String::new()
    };
    (leading, trailing)
}

/// The range the target covers now, provided it still holds what it held when
/// the dialog opened.
fn intact_target_range(snapshot: &BufferSnapshot, target: &ImageTarget) -> Result<Range<usize>> {
    let range = target.range.start.to_offset(snapshot)..target.range.end.to_offset(snapshot);
    let current: String = snapshot.text_for_range(range.clone()).collect();
    if current != target.original_text {
        return Err(anyhow!(
            "ダイアログを開いた後に、書き込み先のテキストが変更されました。ダイアログを開き直してください。"
        ));
    }
    Ok(range)
}

/// Checks the target before anything is copied, so a write that is bound to
/// be refused does not leave a copied picture behind.
fn check_target(editor: &Editor, target: &ImageTarget, cx: &App) -> Result<()> {
    let buffer = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .context("Markdown のバッファーが見つかりません。")?;
    intact_target_range(&buffer.read(cx).snapshot(), target).map(|_| ())
}

/// Replaces the target's range with `markup`, leaving the cursor after it.
fn write_image(
    editor: &mut Editor,
    target: &ImageTarget,
    markup: &ImageMarkup,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Result<()> {
    let buffer = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .context("Markdown のバッファーが見つかりません。")?;
    let snapshot = buffer.read(cx).snapshot();
    let range = intact_target_range(&snapshot, target)?;
    let start_row = snapshot.offset_to_point(range.start).row;
    // What the cursor sat in is left as it was, selection included.
    let (range, first_row) = if target.after_paragraph && markup.is_written_in_a_container() {
        let end_row = paragraph_end_row(&snapshot, snapshot.offset_to_point(range.end).row);
        let end =
            snapshot.point_to_offset(language::Point::new(end_row, snapshot.line_len(end_row)));
        (end..end, paragraph_start_row(&snapshot, start_row))
    } else {
        (range, start_row)
    };
    let line_prefix = line_prefix(&line_text(&snapshot, first_row));
    let rendered = markup.render(&line_prefix.continuation);
    // Inside an HTML block a blank line would end it, so a container there is
    // written back in place as it stood.
    let (prefix, suffix) = if rendered.is_block && !target.inside_html {
        block_padding(&snapshot, &range, &line_prefix)
    } else {
        (String::new(), String::new())
    };
    let cursor = range.start + prefix.len() + rendered.text.len();
    let text = format!("{prefix}{}{suffix}", rendered.text);
    editor.transact(window, cx, |editor, window, cx| {
        editor.edit(
            [(
                MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                text,
            )],
            cx,
        );
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(cursor)..MultiBufferOffset(cursor)]);
        });
    });
    Ok(())
}

/// One image file of the project, as the file list offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageCandidate {
    /// What picking it puts into the path field.
    pub display: String,
    /// Folders between it and the document, which orders the list.
    pub distance: usize,
}

/// What the dialog knows about the file the path field points at.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceDetails {
    /// Nothing to look at: the field is empty or holds a URL.
    None,
    Checking,
    Missing,
    Found {
        preview: Option<PreviewSource>,
        dimensions: Option<(u32, u32)>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreviewSource {
    Local(PathBuf),
    Project(ProjectPath),
}

struct CopyRequest {
    source: PathBuf,
    directory: PathBuf,
}

struct Plan {
    markup: ImageMarkup,
    copy: Option<CopyRequest>,
}

pub struct ImageDialog {
    editor: WeakEntity<Editor>,
    project: WeakEntity<Project>,
    fs: Arc<dyn Fs>,
    location: DocumentLocation,
    target: ImageTarget,
    path_field: Entity<Editor>,
    alt_field: Entity<Editor>,
    width_field: Entity<Editor>,
    height_field: Entity<Editor>,
    alignment: Option<ImageAlignment>,
    copy_into_images: bool,
    /// The alt text this dialog filled in by itself, which it may replace when
    /// another file is picked. Text the user typed is never replaced.
    automatic_alt: Option<String>,
    candidates: Arc<Vec<ImageCandidate>>,
    match_candidates: Arc<Vec<StringMatchCandidate>>,
    candidates_loaded: bool,
    matches: Vec<StringMatch>,
    /// The match the arrow keys moved to. Enter picks it instead of confirming.
    highlighted: Option<usize>,
    candidate_scroll: ScrollHandle,
    details: SourceDetails,
    details_for: Option<String>,
    message: Option<SharedString>,
    copying: bool,
    focus_handle: FocusHandle,
    _candidates_task: Task<()>,
    _match_task: Task<()>,
    _details_task: Task<()>,
    _copy_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ImageDialog {
    pub(crate) fn new(
        editor: WeakEntity<Editor>,
        project: Entity<Project>,
        location: DocumentLocation,
        target: ImageTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let existing = target.existing.clone();
        let path_text = existing
            .as_ref()
            .map(|markup| markup.destination.clone())
            .unwrap_or_default();
        let alt_text = existing
            .as_ref()
            .map(|markup| markup.alt.clone())
            .or_else(|| target.selected_text.clone())
            .unwrap_or_default();
        let width_text = existing
            .as_ref()
            .and_then(|markup| markup.width.clone())
            .unwrap_or_default();
        let height_text = existing
            .as_ref()
            .and_then(|markup| markup.height.clone())
            .unwrap_or_default();

        let mut subscriptions = Vec::new();
        let path_field = new_field(&path_text, "images/photo.png", window, cx);
        let alt_field = new_field(&alt_text, "画像の説明", window, cx);
        let width_field = new_field(&width_text, "例: 300 / 50%", window, cx);
        let height_field = new_field(&height_text, "例: 200", window, cx);
        subscriptions.push(cx.subscribe_in(
            &path_field,
            window,
            |this, _, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.path_changed(window, cx);
                }
            },
        ));
        for field in [&alt_field, &width_field, &height_field] {
            subscriptions.push(cx.subscribe(field, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.message = None;
                    cx.notify();
                }
            }));
        }

        let fs = project.read(cx).fs().clone();
        let candidates_task = Self::collect_candidates(&project, &location, cx);
        let mut this = Self {
            editor,
            project: project.downgrade(),
            fs,
            location,
            alignment: existing.as_ref().and_then(|markup| markup.alignment),
            target,
            path_field,
            alt_field,
            width_field,
            height_field,
            copy_into_images: true,
            automatic_alt: None,
            candidates: Arc::default(),
            match_candidates: Arc::default(),
            candidates_loaded: false,
            matches: Vec::new(),
            highlighted: None,
            candidate_scroll: ScrollHandle::new(),
            details: SourceDetails::None,
            details_for: None,
            message: None,
            copying: false,
            focus_handle: cx.focus_handle(),
            _candidates_task: candidates_task,
            _match_task: Task::ready(()),
            _details_task: Task::ready(()),
            _copy_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.refresh_details(cx);
        this
    }

    fn is_editing(&self) -> bool {
        self.target.existing.is_some()
    }

    /// Lists the image files of every visible worktree, nearest to the
    /// document first. Walking a large worktree is left to the background.
    fn collect_candidates(
        project: &Entity<Project>,
        location: &DocumentLocation,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let worktrees = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (
                    worktree.abs_path().to_string_lossy().into_owned(),
                    worktree.snapshot(),
                )
            })
            .collect::<Vec<_>>();
        let location = location.clone();
        cx.spawn(async move |this, cx| {
            let candidates = cx
                .background_spawn(async move {
                    let style = location.path_style;
                    let directory = location.directory.as_deref();
                    let mut candidates = Vec::new();
                    for (root, snapshot) in &worktrees {
                        for entry in snapshot.files(false, 0) {
                            let relative = entry.path.display(style);
                            if !has_image_extension(&relative) {
                                continue;
                            }
                            let Some(absolute) = join_path(root, &relative, style) else {
                                continue;
                            };
                            let display = directory
                                .and_then(|directory| relative_path(directory, &absolute, style))
                                .unwrap_or_else(|| absolute.replace('\\', "/"));
                            let distance = directory
                                .and_then(|directory| folder_distance(directory, &absolute, style))
                                .unwrap_or(usize::MAX);
                            candidates.push(ImageCandidate { display, distance });
                        }
                    }
                    candidates.sort_by(|left, right| {
                        left.distance
                            .cmp(&right.distance)
                            .then_with(|| left.display.cmp(&right.display))
                    });
                    candidates
                })
                .await;
            this.update(cx, |this, cx| {
                this.match_candidates = Arc::new(
                    candidates
                        .iter()
                        .enumerate()
                        .map(|(index, candidate)| {
                            StringMatchCandidate::new(index, &candidate.display)
                        })
                        .collect(),
                );
                this.candidates = Arc::new(candidates);
                this.candidates_loaded = true;
                this.update_matches(cx);
            })
            .log_err();
        })
    }

    fn path_text(&self, cx: &App) -> String {
        self.path_field.read(cx).text(cx)
    }

    fn path_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.message = None;
        self.highlighted = None;
        let path = self.path_text(cx);
        let alt = self.alt_field.read(cx).text(cx);
        if alt.is_empty() || self.automatic_alt.as_deref() == Some(alt.as_str()) {
            let default = default_alt(&path);
            self.automatic_alt = Some(default.clone());
            if default != alt {
                self.alt_field
                    .update(cx, |field, cx| field.set_text(default, window, cx));
            }
        }
        self.update_matches(cx);
        self.refresh_details(cx);
        cx.notify();
    }

    /// Whether the path field holds something the file list cannot match,
    /// which is when the list gets out of the way.
    fn path_is_outside_the_list(&self, cx: &App) -> bool {
        let path = self.path_text(cx);
        let path = path.trim();
        is_url(path) || self.location.path_style.is_absolute(path)
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.path_text(cx);
        let candidates = self.match_candidates.clone();
        let distances = self.candidates.clone();
        let executor = cx.background_executor().clone();
        self._match_task = cx.spawn(async move |this, cx| {
            let mut matches = fuzzy::match_strings(
                candidates.as_slice(),
                query.trim(),
                false,
                true,
                MAXIMUM_LISTED_CANDIDATES,
                &AtomicBool::new(false),
                executor,
            )
            .await;
            if !query.trim().is_empty() {
                let distance = |candidate_id: usize| {
                    distances
                        .get(candidate_id)
                        .map_or(usize::MAX, |candidate| candidate.distance)
                };
                matches.sort_by(|left, right| {
                    right.score.total_cmp(&left.score).then_with(|| {
                        distance(left.candidate_id).cmp(&distance(right.candidate_id))
                    })
                });
            }
            matches.truncate(MAXIMUM_LISTED_CANDIDATES);
            this.update(cx, |this, cx| {
                this.matches = matches;
                this.highlighted = None;
                cx.notify();
            })
            .log_err();
        });
    }

    fn resolved(&self, cx: &App) -> ResolvedSource {
        resolve_source(&self.path_text(cx), &self.location)
    }

    /// Whether the path is the one the image already had, which is written
    /// back as it was: an edit that only changes the size must not start
    /// copying files or rewriting a path the author chose.
    fn path_is_unchanged(&self, cx: &App) -> bool {
        self.target
            .existing
            .as_ref()
            .is_some_and(|existing| existing.destination == self.path_text(cx))
    }

    fn copy_offered(&self, file: &ResolvedFile, cx: &App) -> bool {
        self.location.is_local
            && self.location.directory.is_some()
            && file.absolute.is_some()
            && !file.inside_root
            && !self.path_is_unchanged(cx)
    }

    /// Looks the file up again when the path field points somewhere new.
    fn refresh_details(&mut self, cx: &mut Context<Self>) {
        let ResolvedSource::File(file) = self.resolved(cx) else {
            self.details = SourceDetails::None;
            self.details_for = None;
            self._details_task = Task::ready(());
            return;
        };
        let Some(absolute) = file.absolute else {
            self.details = SourceDetails::None;
            self.details_for = None;
            self._details_task = Task::ready(());
            return;
        };
        if self.details_for.as_deref() == Some(absolute.as_str()) {
            return;
        }
        self.details_for = Some(absolute.clone());
        self.details = SourceDetails::Checking;
        let is_svg = absolute.to_ascii_lowercase().ends_with(".svg");

        if self.location.is_local {
            let fs = self.fs.clone();
            let path = PathBuf::from(&absolute);
            self._details_task = cx.spawn(async move |this, cx| {
                let is_file = fs
                    .metadata(&path)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|metadata| !metadata.is_dir);
                let details = if is_file {
                    // The picture decoders cannot read an SVG's size, and its
                    // drawing has no pixel size of its own to report.
                    let dimensions = if is_svg {
                        None
                    } else {
                        match fs.load_bytes(&path).await {
                            Ok(bytes) => {
                                cx.background_spawn(async move { image_dimensions(&bytes) })
                                    .await
                            }
                            Err(_) => None,
                        }
                    };
                    SourceDetails::Found {
                        preview: Some(PreviewSource::Local(path)),
                        dimensions,
                    }
                } else {
                    SourceDetails::Missing
                };
                this.update(cx, |this, cx| {
                    this.details = details;
                    cx.notify();
                })
                .log_err();
            });
            return;
        }

        let Some(project) = self.project.upgrade() else {
            return;
        };
        let project_path = project
            .read(cx)
            .find_worktree(Path::new(&absolute), cx)
            .and_then(|(worktree, path)| {
                let worktree = worktree.read(cx);
                worktree
                    .entry_for_path(&path)
                    .is_some_and(|entry| entry.is_file())
                    .then(|| ProjectPath {
                        worktree_id: worktree.id(),
                        path,
                    })
            });
        let Some(project_path) = project_path else {
            // A remote file outside the project cannot be looked at from here,
            // which says nothing about whether it exists.
            self.details = SourceDetails::None;
            self._details_task = Task::ready(());
            return;
        };
        let project = project.downgrade();
        self._details_task = cx.spawn(async move |this, cx| {
            let dimensions = if is_svg {
                None
            } else {
                let open = project.update(cx, |project, cx| {
                    project.open_image(project_path.clone(), cx)
                });
                match open {
                    Ok(open) => match open.await {
                        Ok(image) => {
                            let bytes =
                                image.read_with(cx, |image, _| image.image.bytes().to_vec());
                            cx.background_spawn(async move { image_dimensions(&bytes) })
                                .await
                        }
                        Err(_) => None,
                    },
                    Err(_) => None,
                }
            };
            this.update(cx, |this, cx| {
                this.details = SourceDetails::Found {
                    preview: Some(PreviewSource::Project(project_path)),
                    dimensions,
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn validate(&self, cx: &App) -> Result<Plan, SharedString> {
        let width = self.width_field.read(cx).text(cx);
        let width = match parse_length(&width) {
            LengthInput::Empty => None,
            LengthInput::Valid(width) => Some(width),
            LengthInput::Invalid => {
                return Err(
                    "幅は 300 や 50% のように、数値か % 付きの数値で入力してください。".into(),
                );
            }
        };
        let height = self.height_field.read(cx).text(cx);
        let height = match parse_length(&height) {
            LengthInput::Empty => None,
            LengthInput::Valid(height) => Some(height),
            LengthInput::Invalid => {
                return Err(
                    "高さは 200 や 50% のように、数値か % 付きの数値で入力してください。".into(),
                );
            }
        };

        let mut copy = None;
        let destination = if self.path_is_unchanged(cx) {
            self.path_text(cx)
        } else {
            match self.resolved(cx) {
                ResolvedSource::Empty => return Err("画像ファイルを指定してください。".into()),
                ResolvedSource::Url(url) => url,
                ResolvedSource::File(file) => {
                    if self.copy_offered(&file, cx)
                        && self.copy_into_images
                        && let (Some(source), Some(directory)) =
                            (file.absolute.as_ref(), self.location.directory.as_ref())
                    {
                        copy = Some(CopyRequest {
                            source: PathBuf::from(source),
                            directory: PathBuf::from(directory),
                        });
                        // Filled in once the copy has a name.
                        String::new()
                    } else {
                        file.destination()
                            .ok_or_else(|| SharedString::from("画像ファイルを指定してください。"))?
                    }
                }
            }
        };

        let mut markup = self.target.existing.clone().unwrap_or_default();
        if markup.destination != destination {
            markup.written_destination = None;
        }
        markup.destination = destination;
        markup.alt = single_line(&self.alt_field.read(cx).text(cx));
        markup.width = width;
        markup.height = height;
        markup.alignment = self.alignment;
        markup.keep_html = self.target.inside_html;
        if markup.is_written_in_a_container()
            && let Some(refusal) = self.target.block_refusal
        {
            return Err(refusal.into());
        }
        Ok(Plan { markup, copy })
    }

    /// Notes that do not stop the image from being written.
    fn warnings(&self, cx: &App) -> Vec<SharedString> {
        let mut warnings = Vec::new();
        if matches!(self.details, SourceDetails::Missing) {
            warnings.push("このパスにファイルが見つかりません。".into());
        }
        if self.path_is_unchanged(cx) {
            return warnings;
        }
        if let ResolvedSource::File(file) = self.resolved(cx) {
            let copying = self.copy_offered(&file, cx) && self.copy_into_images;
            if !copying && file.relative.is_none() {
                warnings.push(
                    "Markdown からの相対パスにできないため、絶対パスで書き込みます。他の環境では表示されません。"
                        .into(),
                );
            } else if !copying && self.location.directory.is_none() {
                warnings.push(
                    "Markdown が未保存のため、パスは入力どおりに書き込みます。保存先によっては表示されません。"
                        .into(),
                );
            }
        }
        warnings
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.highlighted
            && self
                .path_field
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            self.pick_candidate(index, window, cx);
            return;
        }
        self.submit(window, cx);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.copying {
            return;
        }
        let plan = match self.validate(cx) {
            Ok(plan) => plan,
            Err(problem) => {
                self.message = Some(problem);
                cx.notify();
                return;
            }
        };
        let Some(copy) = plan.copy else {
            self.write(plan.markup, window, cx);
            return;
        };
        let target = &self.target;
        let checked = self
            .editor
            .read_with(cx, |editor, cx| check_target(editor, target, cx));
        match checked {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.message = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
            Err(_) => {
                cx.emit(DismissEvent);
                return;
            }
        }

        self.copying = true;
        cx.notify();
        let fs = self.fs.clone();
        let mut markup = plan.markup;
        self._copy_task = cx.spawn_in(window, async move |this, cx| {
            let copied = copy_into_images(fs, copy.source, copy.directory).await;
            this.update_in(cx, |this, window, cx| {
                this.copying = false;
                match copied {
                    Ok(file_name) => {
                        markup.destination = format!("images/{file_name}");
                        this.write(markup, window, cx);
                    }
                    Err(error) => {
                        this.message =
                            Some(format!("画像をコピーできませんでした: {error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn write(&mut self, markup: ImageMarkup, window: &mut Window, cx: &mut Context<Self>) {
        let target = &self.target;
        let written = self.editor.update(cx, |editor, cx| {
            write_image(editor, target, &markup, window, cx)
        });
        match written {
            Ok(Ok(())) => cx.emit(DismissEvent),
            Ok(Err(error)) => {
                self.message = Some(format!("{error:#}").into());
                cx.notify();
            }
            // The editor was closed underneath the dialog; there is nothing
            // left to write into.
            Err(_) => cx.emit(DismissEvent),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn pick_candidate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(candidate) = self
            .matches
            .get(index)
            .and_then(|found| self.candidates.get(found.candidate_id))
        else {
            return;
        };
        let display = candidate.display.clone();
        self.path_field.update(cx, |field, cx| {
            field.set_text(display, window, cx);
            field.move_to_end(&MoveToEnd, window, cx);
        });
        self.highlighted = None;
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = window.prompt_for_paths(
            PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("画像を選択".into()),
                starting_directory: self.location.directory.as_ref().map(PathBuf::from),
            },
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        this.message = Some(
                            format!("ファイルを選ぶダイアログを開けませんでした: {error:#}").into(),
                        );
                        cx.notify();
                    })
                    .log_err();
                    None
                }
                _ => None,
            };
            if let Some(path) = picked {
                this.update_in(cx, |this, window, cx| {
                    this.use_picked_file(&path, window, cx)
                })
                .log_err();
            }
        })
        .detach();
    }

    fn use_picked_file(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = path.to_str() else {
            self.message = Some("パスに扱えない文字が含まれています。".into());
            cx.notify();
            return;
        };
        // The file picker cannot be limited to pictures on every platform, so
        // anything else is turned away here, with the reason.
        if !has_image_extension(text) {
            self.message = Some(
                format!(
                    "画像ファイルではありません: {}",
                    path.file_name()
                        .map_or_else(|| text.into(), |name| name.to_string_lossy())
                )
                .into(),
            );
            cx.notify();
            return;
        }
        let text = match resolve_source(text, &self.location) {
            ResolvedSource::File(ResolvedFile {
                relative: Some(relative),
                inside_root: true,
                ..
            }) => relative,
            _ => text.to_string(),
        };
        self.path_field.update(cx, |field, cx| {
            field.set_text(text, window, cx);
            field.move_to_end(&MoveToEnd, window, cx);
        });
    }

    fn fields(&self) -> [Entity<Editor>; 4] {
        [
            self.path_field.clone(),
            self.alt_field.clone(),
            self.width_field.clone(),
            self.height_field.clone(),
        ]
    }

    fn step_field(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields();
        let current = fields
            .iter()
            .position(|field| field.focus_handle(cx).contains_focused(window, cx))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % fields.len()
        } else {
            (current + fields.len() - 1) % fields.len()
        };
        if let Some(field) = fields.get(next) {
            field.focus_handle(cx).focus(window, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn focus_next_field(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_field(true, window, cx);
    }

    fn focus_previous_field(&mut self, _: &Backtab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_field(false, window, cx);
    }

    fn listed_matches(&self, cx: &App) -> usize {
        if self.path_is_outside_the_list(cx) {
            0
        } else {
            self.matches.len()
        }
    }

    fn move_highlight(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.listed_matches(cx);
        if count == 0
            || !self
                .path_field
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            return;
        }
        let next = match (self.highlighted, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(index), true) => (index + 1).min(count - 1),
            (Some(index), false) => index.saturating_sub(1),
        };
        self.highlighted = Some(next);
        self.candidate_scroll.scroll_to_item(next);
        cx.stop_propagation();
        cx.notify();
    }

    fn highlight_next(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(true, window, cx);
    }

    fn highlight_previous(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(false, window, cx);
    }

    fn render_path_row(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self
            .path_field
            .focus_handle(cx)
            .contains_focused(window, cx);
        h_flex()
            .w_full()
            .gap_2()
            .child(
                ui::input_frame(focused, cx)
                    .flex_1()
                    .min_w_0()
                    .child(self.path_field.clone()),
            )
            .when(self.location.is_local, |row| {
                row.child(
                    Button::new("markdown-image-browse", "参照…")
                        .style(ButtonStyle::Outlined)
                        .size(ButtonSize::Medium)
                        .tooltip(Tooltip::text("OS のダイアログで画像ファイルを選びます。"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.browse(window, cx);
                        })),
                )
            })
            .into_any_element()
    }

    fn render_candidates(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.path_is_outside_the_list(cx) {
            return None;
        }
        let note = |text: &'static str| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        if !self.candidates_loaded {
            return Some(note("プロジェクトの画像ファイルを探しています…"));
        }
        if self.candidates.is_empty() {
            return Some(note("プロジェクトに画像ファイルがありません。"));
        }
        if self.matches.is_empty() {
            return Some(note("一致する画像ファイルがありません。"));
        }

        let colors = cx.theme().colors();
        let (selected, hover) = (colors.ghost_element_selected, colors.ghost_element_hover);
        let rows = self
            .matches
            .iter()
            .enumerate()
            .filter_map(|(index, found)| {
                let candidate = self.candidates.get(found.candidate_id)?;
                Some(
                    h_flex()
                        .id(("markdown-image-candidate", index))
                        .w_full()
                        .flex_none()
                        .gap_2()
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(self.highlighted == Some(index), |row| row.bg(selected))
                        .hover(|style| style.bg(hover))
                        .child(
                            Icon::new(IconName::Image)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            HighlightedLabel::new(
                                candidate.display.clone(),
                                found.positions.clone(),
                            )
                            .size(LabelSize::Small),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.pick_candidate(index, window, cx);
                        })),
                )
            });
        Some(
            v_flex()
                .id("markdown-image-candidates")
                .w_full()
                .max_h(rems(9.))
                .overflow_y_scroll()
                .track_scroll(&self.candidate_scroll)
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let (preview, caption) = match &self.details {
            SourceDetails::None => {
                let text = if matches!(self.resolved(cx), ResolvedSource::Url(_)) {
                    "URL の画像はここでは表示しません。"
                } else {
                    "画像を選ぶとここに表示します。"
                };
                (None, Some(text.to_string()))
            }
            SourceDetails::Checking => (None, Some("確認しています…".to_string())),
            SourceDetails::Missing => (None, None),
            SourceDetails::Found {
                preview,
                dimensions,
            } => (
                preview.clone(),
                dimensions.map(|(width, height)| format!("元のサイズ: {width} × {height}")),
            ),
        };
        let thumbnail = preview.map(|preview| {
            let source = match preview {
                PreviewSource::Local(path) => {
                    ImageSource::Resource(Resource::Path(Arc::from(path)))
                }
                PreviewSource::Project(project_path) => {
                    image_store::project_image_source(self.project.clone(), project_path)
                }
            };
            img(source)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(|| {
                    Label::new("画像を読み込めません")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .into_any_element()
                })
                .into_any_element()
        });
        let colors = cx.theme().colors();

        h_flex()
            .w_full()
            .gap_3()
            .items_start()
            .child(
                div()
                    .flex_none()
                    .w(rems(10.))
                    .h(rems(7.5))
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(thumbnail),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .children(caption.map(|caption| {
                        Label::new(caption)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    }))
                    .children(self.warnings(cx).into_iter().map(|warning| {
                        Label::new(warning)
                            .size(LabelSize::Small)
                            .color(Color::Warning)
                    })),
            )
            .into_any_element()
    }

    /// Says where an aligned image typed in the middle of a line goes, since
    /// that is not where the cursor is.
    fn placement_note(&self) -> Option<&'static str> {
        if !self.target.after_paragraph || self.alignment.is_none() {
            return None;
        }
        Some(if self.target.original_text.is_empty() {
            "配置を指定した画像は、カーソルのある段落の後ろに入ります。"
        } else {
            "配置を指定した画像は、カーソルのある段落の後ろに入ります。選択した文字列はそのまま残ります。"
        })
    }

    fn render_alignment(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices: [(Option<ImageAlignment>, &'static str); 4] = [
            (None, "指定なし"),
            (Some(ImageAlignment::Left), "左"),
            (Some(ImageAlignment::Center), "中央"),
            (Some(ImageAlignment::Right), "右"),
        ];
        let selected_index = choices
            .iter()
            .position(|(alignment, _)| *alignment == self.alignment)
            .unwrap_or(0);
        let buttons = choices.map(|(alignment, label)| {
            ToggleButtonSimple::new(
                label,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.alignment = alignment;
                    this.message = None;
                    cx.notify();
                }),
            )
            .selected(alignment == self.alignment)
        });
        ToggleButtonGroup::single_row("markdown-image-alignment", buttons)
            .style(ToggleButtonGroupStyle::Outlined)
            .auto_width()
            .selected_index(selected_index)
            .into_any_element()
    }

    fn render_size(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let field = |label: &'static str, field: &Entity<Editor>, cx: &App| {
            let focused = field.focus_handle(cx).contains_focused(window, cx);
            h_flex()
                .gap_2()
                .child(Label::new(label).size(LabelSize::Small))
                .child(
                    ui::input_frame(focused, cx)
                        .w(rems(8.))
                        .child(field.clone()),
                )
        };
        let one_side = self.width_field.read(cx).text(cx).trim().is_empty()
            != self.height_field.read(cx).text(cx).trim().is_empty();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_4()
                    .child(field("幅", &self.width_field, cx))
                    .child(field("高さ", &self.height_field, cx)),
            )
            .child(
                Label::new(if one_side {
                    "片方だけを指定すると、縦横比は保たれます。"
                } else {
                    "数値 (px) か % 付きで入力します。空欄なら指定しません。"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_copy_option(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ResolvedSource::File(file) = self.resolved(cx) else {
            return None;
        };
        if !self.copy_offered(&file, cx) {
            return None;
        }
        Some(
            Checkbox::new("markdown-image-copy", self.copy_into_images.into())
                .label("Markdown と同じフォルダーの images/ にコピーする")
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                    this.copy_into_images = state.selected();
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    fn render_footer(&self, problem: Option<SharedString>, cx: &mut Context<Self>) -> ModalFooter {
        let confirm_label = if self.is_editing() {
            "更新"
        } else {
            "挿入"
        };
        let status = self
            .message
            .clone()
            .map(|message| (message, Color::Warning))
            .or_else(|| problem.clone().map(|problem| (problem, Color::Muted)))
            .or_else(|| {
                self.copying
                    .then(|| ("画像をコピーしています…".into(), Color::Muted))
            });
        ModalFooter::new()
            .start_slot::<AnyElement>(
                div()
                    .min_w_0()
                    .children(
                        status.map(|(text, color)| {
                            Label::new(text).size(LabelSize::Small).color(color)
                        }),
                    )
                    .into_any_element(),
            )
            .end_slot::<AnyElement>(
                h_flex()
                    .gap_1()
                    .child(Button::new("markdown-image-cancel", "キャンセル").on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.cancel(&menu::Cancel, window, cx)
                        }),
                    ))
                    .child(
                        Button::new("markdown-image-confirm", confirm_label)
                            .style(ButtonStyle::Filled)
                            .disabled(problem.is_some() || self.copying)
                            .when_some(problem, |button, problem| {
                                button.tooltip(Tooltip::text(problem))
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit(window, cx)
                            })),
                    )
                    .into_any_element(),
            )
    }
}

/// The pixel size of an encoded picture, when the decoders can read it.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageItem::compute_metadata_from_bytes(bytes)
        .ok()
        .map(|metadata| (metadata.width, metadata.height))
}

fn new_field(text: &str, placeholder: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    let settings = ThemeSettings::get_global(cx);
    // The fields hold text that ends up in the document, so they follow the
    // buffer font the user already sized to be readable, as the table and
    // Mermaid editors do.
    let text_style = TextStyleRefinement {
        font_family: Some(settings.buffer_font.family.clone()),
        font_features: Some(settings.buffer_font.features.clone()),
        font_size: Some(settings.buffer_font_size(cx).into()),
        font_weight: Some(settings.buffer_font.weight),
        ..Default::default()
    };
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text(text.to_string(), window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text_style_refinement(text_style);
        editor
    })
}

/// Text from a one-line field, which a paste can still fill with line breaks:
/// a blank line would end the image or split its HTML block, and a single one
/// reads as a space in both anyway.
fn single_line(text: &str) -> String {
    if !text.contains(['\n', '\r']) {
        return text.to_string();
    }
    text.split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn section_label(text: &'static str) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

impl Render for ImageDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let problem = self.validate(cx).err();
        let (headline, description) = if self.is_editing() {
            (
                "画像を編集",
                "カーソル位置の画像を読み込みました。変更して「更新」を押すと書き換えます。",
            )
        } else {
            (
                "画像を挿入",
                "カーソル位置に画像を挿入します。配置やサイズを指定すると HTML で書き込みます。",
            )
        };
        let path_row = self.render_path_row(window, cx);
        let candidates = self.render_candidates(cx);
        let details = self.render_details(cx);
        let alignment = self.render_alignment(cx);
        let size = self.render_size(window, cx);
        let copy_option = self.render_copy_option(cx);
        let alt_focused = self.alt_field.focus_handle(cx).contains_focused(window, cx);
        let footer = self.render_footer(problem, cx);

        div()
            .key_context("MarkdownImageDialog")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(36.))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .capture_action(cx.listener(Self::focus_next_field))
            .capture_action(cx.listener(Self::focus_previous_field))
            .capture_action(cx.listener(Self::highlight_next))
            .capture_action(cx.listener(Self::highlight_previous))
            .child(
                Modal::new("markdown-image-dialog", None)
                    .header(
                        ModalHeader::new()
                            .headline(headline)
                            .description(description),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .id("markdown-image-form")
                                .w_full()
                                .max_h(window.viewport_size().height * MAXIMUM_FORM_HEIGHT_FRACTION)
                                .overflow_y_scroll()
                                .gap_2()
                                .child(section_label("画像ファイル"))
                                .child(path_row)
                                .children(candidates)
                                .children(copy_option)
                                .child(details)
                                .child(section_label("代替テキスト"))
                                .child(
                                    ui::input_frame(alt_focused, cx)
                                        .w_full()
                                        .child(self.alt_field.clone()),
                                )
                                .child(section_label("配置"))
                                .child(alignment)
                                .children(self.placement_note().map(|note| {
                                    Label::new(note).size(LabelSize::Small).color(Color::Muted)
                                }))
                                .child(section_label("サイズ"))
                                .child(size),
                        ),
                    )
                    .footer(footer),
            )
    }
}

impl Focusable for ImageDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.path_field.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ImageDialog {}

impl ModalView for ImageDialog {
    /// The modal holds anchors into the buffer it will write to; reviving a
    /// stale copy later would write against text that has moved on.
    fn stash_when_dismissed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests;
