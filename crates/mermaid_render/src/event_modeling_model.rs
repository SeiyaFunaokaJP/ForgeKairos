//! A round-trippable editing model for Mermaid event models (`eventmodeling`).
//!
//! The shape of this module mirrors [`crate::pie_model`]: [`parse_event_modeling`]
//! reads the body of a ```` ```mermaid ```` fence into [`EventModelingDocument`],
//! and [`emit_event_modeling`] writes it back. See [`crate::mermaid_model`] for
//! the invariants every diagram kind keeps.
//!
//! `merman` 0.7 read an event model one line at a time and silently skipped
//! what it did not recognise; 0.8 reads it the way Mermaid's grammar does and
//! refuses what it does not recognise. The model reads the source itself,
//! refuses what it does not carry (a `note`, a `gwt`, an `entity`, and the
//! `title` and accessibility lines only `merman` takes — Mermaid refuses
//! them), and checks the result against `merman`'s own reading before the
//! editor opens.
//!
//! Mermaid reads names as ASCII identifiers: a frame's number is one to three
//! digits, a lane, a box's name and a data block's name are letters, digits
//! and `_`, starting with a letter or `_`. A data block is `{` … `}` over
//! several lines, closed by a `}` at the start of its own line.
//!
//! The frames are a time line — their order is the order of the boxes — so the
//! lines the model does not carry travel with the statement they stand in front
//! of instead of being gathered at one end.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADER: &str = "eventmodeling";
const SOURCE_ARROW: &str = "->>";

/// What a frame's box stands for. It decides the colour of the box and the
/// swim lane it is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventModelingEntityType {
    Ui,
    Processor,
    ReadModel,
    Command,
    Event,
}

impl EventModelingEntityType {
    pub const ALL: [Self; 5] = [
        Self::Ui,
        Self::Processor,
        Self::Command,
        Self::Event,
        Self::ReadModel,
    ];

    /// The short keyword, which is what the form writes for a type it sets.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Processor => "pcr",
            Self::ReadModel => "rmo",
            Self::Command => "cmd",
            Self::Event => "evt",
        }
    }

    /// The spelled-out keyword `merman` takes as a synonym.
    pub fn long_keyword(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Processor => "processor",
            Self::ReadModel => "readmodel",
            Self::Command => "command",
            Self::Event => "event",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ui => i18n::t!("Screen (ui)"),
            Self::Processor => i18n::t!("Automation (pcr)"),
            Self::ReadModel => i18n::t!("Read model (rmo)"),
            Self::Command => i18n::t!("Command (cmd)"),
            Self::Event => i18n::t!("Event (evt)"),
        }
    }

    /// The type a keyword names, and whether it is the spelled-out synonym.
    ///
    /// `merman` draws any other word as a red box in the event lane, which is
    /// never what was meant, so the model does not take one.
    pub fn from_keyword(keyword: &str) -> Option<(Self, bool)> {
        Self::ALL.into_iter().find_map(|kind| {
            if keyword == kind.keyword() {
                Some((kind, false))
            } else if keyword == kind.long_keyword() {
                Some((kind, true))
            } else {
                None
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventModelingFrame {
    /// The frame's name — the `01` of `tf 01 ui 画面` — which arrows address.
    pub name: String,
    /// A reset frame (`rf`) starts a new chain: no arrow is drawn into it.
    pub reset: bool,
    /// Whether the source spells the keyword out (`timeframe`, `resetframe`).
    pub long_keyword: bool,
    pub entity_type: EventModelingEntityType,
    /// Whether the source spells the type out (`event`, `command`, …).
    pub long_type: bool,
    /// The swim lane: the `Cart` of `Cart.ItemAdded`. Frames of one type and
    /// one namespace share a lane.
    pub namespace: Option<String>,
    /// What the box says: the identifier without its namespace.
    pub label: String,
    /// The frames (by name) arrows are drawn from. Empty means `merman` draws
    /// one from the nearest earlier frame in another lane by itself.
    pub sources: Vec<String>,
    /// Data written on the line itself, braces included: `{ sku: 1 }`.
    pub inline_data: Option<String>,
    /// The `data` block (by name) whose contents the box shows: `[[Name]]`.
    pub data_reference: Option<String>,
    /// Comments and lines `merman` ignores that stand before this frame,
    /// verbatim.
    pub leading_lines: Vec<String>,
}

impl EventModelingFrame {
    /// The identifier as the frame line spells it.
    pub fn identifier(&self) -> String {
        match &self.namespace {
            Some(namespace) => format!("{namespace}.{}", self.label),
            None => self.label.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventModelingDataBlock {
    pub name: String,
    /// The block, braces included. Lines after the first keep their
    /// indentation; trailing whitespace is not kept.
    pub value: String,
    pub leading_lines: Vec<String>,
}

/// An editable event model.
#[derive(Debug, Clone, PartialEq)]
pub struct EventModelingDocument {
    pub frames: Vec<EventModelingFrame>,
    pub data_blocks: Vec<EventModelingDataBlock>,
    /// Frontmatter and directives before the keyword
    /// ([`PreservedPosition::Preamble`]) and whatever follows the last
    /// statement ([`PreservedPosition::Bottom`]).
    pub preserved_lines: Vec<PreservedLine>,
}

impl EventModelingDocument {
    /// A blank event model, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            frames: Vec::new(),
            data_blocks: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// the display language, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        for (index, frame) in self.frames.iter().enumerate() {
            let earlier = || self.frames.iter().take(index);
            if !is_valid_frame_name(&frame.name) {
                return Some(i18n::t!(
                    "The frame number `{}` cannot be used (1 to 3 ASCII digits).",
                    frame.name
                ));
            }
            if earlier().any(|other| other.name == frame.name) {
                return Some(i18n::t!(
                    "The frame number `{}` is used more than once.",
                    frame.name
                ));
            }
            if !is_valid_frame_label(&frame.label) {
                return Some(i18n::t!(
                    "The name `{}` cannot be used (ASCII letters, digits and `_`, starting with a letter or `_`. It can be split with `.`, but without a lane a name cannot contain exactly one `.`).",
                    frame.label
                ));
            }
            if let Some(namespace) = &frame.namespace {
                if !is_valid_namespace(namespace) {
                    return Some(i18n::t!(
                        "The lane `{namespace}` cannot be used (ASCII letters, digits and `_`, starting with a letter or `_`)."
                    ));
                }
                if frame.label.contains('.') {
                    return Some(i18n::t!(
                        "The name `{}` has a lane, so it cannot contain `.`.",
                        frame.label
                    ));
                }
            }
            if let Some(data) = &frame.inline_data
                && !is_valid_inline_data(data)
            {
                return Some(i18n::t!(
                    "The data `{data}` of frame `{}` cannot be written (a single line enclosed in {{ }}; no %%).",
                    frame.name
                ));
            }
            if frame.inline_data.is_some() && frame.data_reference.is_some() {
                return Some(i18n::t!(
                    "Frame `{}` has both inline data and a data reference. Keep only one.",
                    frame.name
                ));
            }
            if let Some(reference) = &frame.data_reference
                && !self
                    .data_blocks
                    .iter()
                    .any(|block| &block.name == reference)
            {
                return Some(i18n::t!(
                    "Frame `{}` refers to the data `{reference}`, which does not exist.",
                    frame.name
                ));
            }
            for (source_index, source) in frame.sources.iter().enumerate() {
                if source == &frame.name {
                    return Some(i18n::t!(
                        "The arrow of frame `{}` points to itself.",
                        frame.name
                    ));
                }
                // `merman` looks the source up among the frames drawn so far
                // and drops the arrow when it is not there yet.
                if !earlier().any(|other| &other.name == source) {
                    return Some(i18n::t!(
                        "The source `{source}` of the arrow to frame `{}` is not in an earlier frame.",
                        frame.name
                    ));
                }
                if frame
                    .sources
                    .iter()
                    .take(source_index)
                    .any(|other| other == source)
                {
                    return Some(i18n::t!(
                        "There are two arrows from `{source}` to `{}`.",
                        frame.name
                    ));
                }
            }
        }
        for (index, block) in self.data_blocks.iter().enumerate() {
            if !is_valid_data_name(&block.name) {
                return Some(i18n::t!(
                    "The data name `{}` cannot be used (ASCII letters, digits and `_`, starting with a letter or `_`).",
                    block.name
                ));
            }
            if self
                .data_blocks
                .iter()
                .take(index)
                .any(|other| other.name == block.name)
            {
                return Some(i18n::t!(
                    "The data name `{}` is used more than once.",
                    block.name
                ));
            }
            if !is_valid_data_value(&block.value) {
                return Some(i18n::t!(
                    "The contents of data `{}` must start on the {{ line and end with a line that is only }} at the start of the line (it cannot be written on one line; no %%).",
                    block.name
                ));
            }
        }
        None
    }
}

/// Whether `source` is an event model: its first statement is `eventmodeling`.
pub fn is_event_modeling_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_event_modeling(source: &str) -> Result<EventModelingDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = EventModelingDocument {
        frames: Vec::new(),
        data_blocks: Vec::new(),
        preserved_lines: preamble.preserved,
    };

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };
    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, HEADER) else {
        bail!(i18n::t!(
            "This is not an event modeling diagram (only diagrams that start with `eventmodeling` can be edited in the structured editor)."
        ));
    };
    if !tail.is_empty() {
        bail!(i18n::t!(
            "Mermaid reads the rest of the `eventmodeling` line ({tail}) as the first frame. Move it to the next line, then open it again."
        ));
    }

    let mut leading = Vec::new();
    let mut index = header_index + 1;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        let start = index;
        index += 1;
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            leading.push(join_lines(lines.get(start..index).unwrap_or_default()));
            continue;
        }
        if let Some(rest) = keyword_tail(trimmed, "data") {
            let (block, next) = parse_data_block(rest, &lines, index)?;
            index = next;
            document.data_blocks.push(EventModelingDataBlock {
                leading_lines: std::mem::take(&mut leading),
                ..block
            });
            continue;
        }
        if let Some(mut frame) = parse_frame(trimmed)? {
            frame.leading_lines = std::mem::take(&mut leading);
            document.frames.push(frame);
            continue;
        }
        let keyword = trimmed
            .split(|character: char| character.is_whitespace() || character == ':')
            .next()
            .unwrap_or_default();
        match keyword {
            "note" | "gwt" | "entity" => bail!(i18n::t!(
                "The `{keyword}` statement is not supported by the structured editor (line {}). Edit it as text.",
                start + 1
            )),
            "title" | "accTitle" | "accDescr" => bail!(i18n::t!(
                "`{keyword}` cannot be used in event modeling (line {}; the official Mermaid does not accept it). Remove the line, or edit it as text.",
                start + 1
            )),
            // `merman` 0.7 skipped a line it did not recognise; 0.8 refuses
            // it, as Mermaid does, and reports where.
            _ => {
                if let Err(error) = crate::parse_for_render_model(source) {
                    bail!(i18n::t!(
                        "Mermaid could not parse this diagram ({error}). Edit it as text."
                    ));
                }
                bail!(i18n::t!(
                    "`{trimmed}` is not supported by the structured editor (line {}). Edit it as text.",
                    start + 1
                ));
            }
        }
    }
    document
        .preserved_lines
        .extend(leading.into_iter().map(|text| PreservedLine {
            position: PreservedPosition::Bottom,
            text,
        }));

    check_against_merman(source, &document)?;
    Ok(document)
}

/// Reads a `tf` / `timeframe` / `rf` / `resetframe` line, or answers `None` for
/// a line that is none of them.
fn parse_frame(line: &str) -> Result<Option<EventModelingFrame>> {
    let (reset, long_keyword, rest) = if let Some(rest) = frame_keyword(line, "tf") {
        (false, false, rest)
    } else if let Some(rest) = frame_keyword(line, "timeframe") {
        (false, true, rest)
    } else if let Some(rest) = frame_keyword(line, "rf") {
        (true, false, rest)
    } else if let Some(rest) = frame_keyword(line, "resetframe") {
        (true, true, rest)
    } else {
        return Ok(None);
    };

    // `merman` 0.8 reads the words as tokens, so any run of whitespace
    // separates them.
    let mut remaining = rest;
    let mut next_word = |what: &str| -> Result<String> {
        let trimmed = remaining.trim_start();
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let word = trimmed.get(..end).unwrap_or_default();
        remaining = trimmed.get(end..).unwrap_or_default();
        if word.is_empty() {
            bail!(i18n::t!(
                "The frame line has no {what} ({line}). Write it as `tf number type name`."
            ));
        }
        Ok(word.to_string())
    };
    let name = next_word(i18n::tc!("event modeling frame field", "number"))?;
    let type_keyword = next_word(i18n::tc!("event modeling frame field", "type"))?;
    let identifier = next_word(i18n::tc!("event modeling frame field", "name"))?;
    let tail = remaining.trim();

    let Some((entity_type, long_type)) = EventModelingEntityType::from_keyword(&type_keyword)
    else {
        bail!(i18n::t!(
            "Mermaid does not know the type `{type_keyword}` and only draws it as a red box ({line}). Use one of ui / pcr / cmd / evt / rmo."
        ));
    };
    if !is_valid_frame_name(&name) {
        bail!(i18n::t!(
            "The frame number `{name}` cannot be handled by the structured editor ({line})."
        ));
    }
    if identifier.contains("%%") {
        bail!(i18n::t!(
            "A frame whose name contains %% cannot be handled by the structured editor ({line})."
        ));
    }
    let (namespace, label) = split_identifier(&identifier);

    let mut frame = EventModelingFrame {
        name,
        reset,
        long_keyword,
        entity_type,
        long_type,
        namespace,
        label,
        sources: Vec::new(),
        inline_data: None,
        data_reference: None,
        leading_lines: Vec::new(),
    };
    parse_frame_tail(tail, line, &mut frame)?;
    Ok(Some(frame))
}

fn frame_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

/// Splits an identifier into its lane and its label the way `merman` does: a
/// namespace only when there is exactly one `.` with text on both sides.
fn split_identifier(identifier: &str) -> (Option<String>, String) {
    let mut parts = identifier.split('.');
    if let (Some(namespace), Some(label), None) = (parts.next(), parts.next(), parts.next())
        && !namespace.is_empty()
        && !label.is_empty()
    {
        return (Some(namespace.to_string()), label.to_string());
    }
    (None, identifier.to_string())
}

/// Reads what follows the identifier: arrows (`->> 01`), inline data
/// (`{ … }`) and a data reference (`[[Name]]`), in any order.
///
/// `merman` searches the whole tail for each of them independently and
/// ignores every other word, so a word this does not recognise is refused
/// rather than dropped.
fn parse_frame_tail(tail: &str, line: &str, frame: &mut EventModelingFrame) -> Result<()> {
    let mut rest = tail;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Ok(());
        }
        if let Some(after) = rest.strip_prefix(SOURCE_ARROW)
            && (after.is_empty() || after.starts_with(char::is_whitespace))
        {
            let after = after.trim_start();
            let length = after.find(char::is_whitespace).unwrap_or(after.len());
            let word = after.get(..length).unwrap_or_default();
            let source = word.trim_matches(|character| character == ',' || character == ';');
            if !is_valid_frame_name(source) {
                bail!(i18n::t!("There is no frame number after `->>` ({line})."));
            }
            frame.sources.push(source.to_string());
            rest = after.get(length..).unwrap_or_default();
            continue;
        }
        if let Some(after) = rest.strip_prefix("[[") {
            let Some(end) = after.find("]]") else {
                bail!(i18n::t!("There is no `]]` closing `[[` ({line})."));
            };
            let reference = after.get(..end).unwrap_or_default().trim();
            if frame.inline_data.is_some() || frame.data_reference.is_some() {
                bail!(i18n::t!(
                    "Both inline data and a data reference are written ({line}). Keep only one, then open it again."
                ));
            }
            if !is_valid_data_name(reference) {
                bail!(i18n::t!(
                    "The data reference `[[{reference}]]` cannot be handled by the structured editor ({line})."
                ));
            }
            frame.data_reference = Some(reference.to_string());
            rest = after.get(end + 2..).unwrap_or_default();
            continue;
        }
        if rest.starts_with('{') {
            let Some(length) = balanced_length(rest) else {
                bail!(i18n::t!("The `{{` of the data is not closed ({line})."));
            };
            if frame.inline_data.is_some() || frame.data_reference.is_some() {
                bail!(i18n::t!(
                    "Both inline data and a data reference are written ({line}). Keep only one, then open it again."
                ));
            }
            frame.inline_data = Some(rest.get(..length).unwrap_or_default().to_string());
            rest = rest.get(length..).unwrap_or_default();
            continue;
        }
        if rest.starts_with("%%") {
            bail!(i18n::t!(
                "A comment at the end of a frame line is lost in the structured editor ({line}). Move the comment to a line of its own, then open it again."
            ));
        }
        let word = rest.split_whitespace().next().unwrap_or(rest);
        bail!(i18n::t!(
            "Mermaid cannot read `{word}` on the frame line ({line}). Names cannot contain spaces, so remove it or make it one word, then open it again."
        ));
    }
}

/// The length of the `{ … }` group `text` opens with, or `None` when the braces
/// never balance.
fn balanced_length(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in text.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn brace_delta(text: &str) -> isize {
    text.chars().fold(0, |depth, character| match character {
        '{' => depth + 1,
        '}' => depth - 1,
        _ => depth,
    })
}

/// Reads a `data` block whose keyword line came just before `index`, taking
/// the following lines exactly as `merman` does, and returns the block with the
/// index of the first line after it.
fn parse_data_block(
    rest: &str,
    lines: &[&str],
    mut index: usize,
) -> Result<(EventModelingDataBlock, usize)> {
    let rest = rest.trim_start();
    let length = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = rest.get(..length).unwrap_or_default();
    if name.is_empty() {
        bail!(i18n::t!("There is no data name after `data`."));
    }
    if !is_valid_data_name(name) {
        bail!(i18n::t!(
            "The data name `{name}` cannot be handled by the structured editor."
        ));
    }
    let first_tail = rest.get(length..).unwrap_or_default().trim();

    // `merman` keeps reading lines until it has seen a `{` and the braces
    // balance again, whatever those lines say.
    let mut block = first_tail.to_string();
    let mut depth = brace_delta(&block);
    let take_line = |block: &mut String, depth: &mut isize, index: &mut usize| -> Result<bool> {
        let Some(next) = lines.get(*index).copied() else {
            return Ok(false);
        };
        // Comment lines are gone before `merman`'s parser sees the block, so
        // one inside it would fall out of it on the next write.
        if next.trim_start().starts_with("%%") {
            bail!(i18n::t!(
                "The comment line ({}) inside data `{name}` is lost in the structured editor. Move it outside the block, then open it again.",
                next.trim()
            ));
        }
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(next);
        *depth += brace_delta(next);
        *index += 1;
        Ok(true)
    };
    while depth <= 0 || !block.contains('{') {
        if !take_line(&mut block, &mut depth, &mut index)? {
            break;
        }
        if block.contains('{') && depth <= 0 {
            break;
        }
    }
    while depth > 0 {
        if !take_line(&mut block, &mut depth, &mut index)? {
            break;
        }
    }

    let value = normalize_data_value(block.trim());
    if value.is_empty() {
        bail!(i18n::t!("Data `{name}` has no contents."));
    }
    if !is_valid_data_value(&value) {
        // A block that would be fine without its comments is refused for the
        // comments alone, and the advice about one-line blocks would only
        // mislead.
        let without_comments = value
            .lines()
            .map(|line| {
                line.find("%%")
                    .map_or(line, |start| line.get(..start).unwrap_or_default())
                    .trim_end()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if value.contains("%%") && is_valid_data_value(without_comments.trim()) {
            bail!(i18n::t!(
                "A comment (`%%`) on a line of data `{name}` is lost in the structured editor. Move the comment to a line of its own outside the block, then open it again."
            ));
        }
        bail!(i18n::t!(
            "Data `{name}` cannot be handled by the structured editor. Start it with the line `data {name} {{` and end it with a line that is only `}}` at the start (Mermaid does not accept a one-line block)."
        ));
    }
    Ok((
        EventModelingDataBlock {
            name: name.to_string(),
            value,
            leading_lines: Vec::new(),
        },
        index,
    ))
}

fn normalize_data_value(value: &str) -> String {
    value
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Confirms that `merman` reads the source into the same frames, arrows and
/// data blocks the document holds. The line reading above mirrors `merman`'s,
/// and this keeps a difference between the two from being written back.
fn check_against_merman(source: &str, document: &EventModelingDocument) -> Result<()> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(i18n::t!(
                "Mermaid could not parse this diagram ({error}). Edit it as text."
            ))
        })?
        .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
    let RenderSemanticModel::EventModeling(model) = parsed.into_parts().1 else {
        bail!(i18n::t!("This diagram is not an event modeling diagram."));
    };

    let mismatch = || {
        anyhow!(i18n::t!(
            "Mermaid and the structured editor read this diagram differently. Edit it as text."
        ))
    };
    // Preprocessing turns `="…"` into `='…'` between a `<tag` and the next
    // `>` anywhere after it, even one on a later line, so a text without `<`
    // of its own can still come back with its quotes changed.
    let quotes_rewritten = source.contains('<') && source.contains("=\"");
    let is_comparable =
        |text: &str| is_plain_text(text) && !(quotes_rewritten && text.contains('"'));
    if model.frames.len() != document.frames.len()
        || model.data_entities.len() != document.data_blocks.len()
    {
        return Err(mismatch());
    }
    for (theirs, ours) in model.frames.iter().zip(&document.frames) {
        let kind = if ours.reset {
            "resetframe"
        } else {
            "timeframe"
        };
        let type_keyword = if ours.long_type {
            ours.entity_type.long_keyword()
        } else {
            ours.entity_type.keyword()
        };
        if theirs.name != ours.name
            || theirs.frame_kind != kind
            || theirs.model_entity_type != type_keyword
            || theirs.source_frames != ours.sources
            || theirs.data_reference != ours.data_reference
            || theirs.data_inline_value.is_some() != ours.inline_data.is_some()
        {
            return Err(mismatch());
        }
        // Entities and HTML are rewritten on the way in, so the texts are only
        // compared where neither can have happened.
        let identifier = ours.identifier();
        if is_comparable(&identifier) && theirs.entity_identifier != identifier {
            return Err(mismatch());
        }
        if let (Some(theirs), Some(ours)) = (&theirs.data_inline_value, &ours.inline_data)
            && is_comparable(ours)
            && theirs != ours
        {
            return Err(mismatch());
        }
    }
    for (theirs, ours) in model.data_entities.iter().zip(&document.data_blocks) {
        if theirs.name != ours.name
            || (is_comparable(&ours.value)
                && normalize_data_value(&theirs.data_block_value) != ours.value)
        {
            return Err(mismatch());
        }
    }
    Ok(())
}

fn is_plain_text(text: &str) -> bool {
    !text.contains(['#', '<', '>', '&'])
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_event_modeling(document: &EventModelingDocument) -> String {
    let mut out = String::new();
    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(HEADER);
    out.push('\n');

    for frame in &document.frames {
        for line in &frame.leading_lines {
            out.push_str(line);
            out.push('\n');
        }
        let keyword = match (frame.reset, frame.long_keyword) {
            (false, false) => "tf",
            (false, true) => "timeframe",
            (true, false) => "rf",
            (true, true) => "resetframe",
        };
        let type_keyword = if frame.long_type {
            frame.entity_type.long_keyword()
        } else {
            frame.entity_type.keyword()
        };
        out.push_str(&format!(
            "{INDENT}{keyword} {} {type_keyword} {}",
            frame.name,
            frame.identifier()
        ));
        for source in &frame.sources {
            out.push_str(&format!(" {SOURCE_ARROW} {source}"));
        }
        if let Some(data) = &frame.inline_data {
            out.push(' ');
            out.push_str(data);
        } else if let Some(reference) = &frame.data_reference {
            out.push_str(&format!(" [[{reference}]]"));
        }
        out.push('\n');
    }

    for block in &document.data_blocks {
        for line in &block.leading_lines {
            out.push_str(line);
            out.push('\n');
        }
        let (first, rest) = block
            .value
            .split_once('\n')
            .unwrap_or((block.value.as_str(), ""));
        // A block whose `{` line closes its own braces goes on the line after
        // the name, where Mermaid reads it (a one-line block is refused by
        // `is_valid_data_value`).
        if brace_delta(first) > 0 {
            out.push_str(&format!("{INDENT}data {} {first}\n", block.name));
            if !rest.is_empty() {
                out.push_str(rest);
                out.push('\n');
            }
        } else {
            out.push_str(&format!("{INDENT}data {}\n", block.name));
            out.push_str(&format!("{INDENT}{}\n", block.value));
        }
    }

    for line in preserved_at(document, PreservedPosition::Bottom) {
        out.push_str(line);
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn preserved_at(
    document: &EventModelingDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `name` is an identifier as Mermaid (and `merman` 0.8) reads one:
/// ASCII letters, digits and `_`, starting with a letter or `_`.
fn is_identifier(name: &str) -> bool {
    name.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether `name` can be a frame's number (the `01` of `tf 01 ui Screen`):
/// one to three ASCII digits.
pub fn is_valid_frame_name(name: &str) -> bool {
    (1..=3).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether `name` can name a `data` block.
pub fn is_valid_data_name(name: &str) -> bool {
    is_identifier(name)
}

/// Whether `namespace` can be a frame's lane.
pub fn is_valid_namespace(namespace: &str) -> bool {
    is_identifier(namespace)
}

/// Whether `label` can be what a frame's box says: identifiers joined by `.`.
/// Without a lane, a single `.` would make its first half one.
pub fn is_valid_frame_label(label: &str) -> bool {
    label.split('.').all(is_identifier) && split_identifier(label).0.is_none()
}

/// Whether `data` can be written as a frame's inline data: one `{ … }` group
/// on one line. Mermaid reads it to the last `}` of the line, so an arrow or
/// a `[[…]]` inside it is data.
pub fn is_valid_inline_data(data: &str) -> bool {
    data.starts_with('{')
        && balanced_length(data) == Some(data.len())
        && !data.contains(['\n', '\r'])
        && !data.contains("%%")
}

/// Whether `value` can be written as a `data` block: a `{ … }` group whose
/// braces close only at its very end, over several lines — Mermaid ends the
/// block at the first `}` that starts a line, and refuses one on a single
/// line — without the comment marker preprocessing would cut out of it.
pub fn is_valid_data_value(value: &str) -> bool {
    let closes_on_its_own_line = value
        .strip_suffix("\n}")
        .is_some_and(|body| !body.contains("\n}"));
    value.starts_with('{')
        && balanced_length(value) == Some(value.len())
        && closes_on_its_own_line
        && !value.contains("%%")
        && !value.contains('\r')
        && value.lines().all(|line| line.trim_end() == line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str = "eventmodeling
    %% tf 番号 種類 名前。ui は画面、cmd は操作、evt は出来事、rmo は読み取りモデル。名前は半角英数字で書く (Mermaid は日本語の名前を読めない)
    tf 01 ui CartScreen
    tf 02 cmd AddItem
    tf 03 evt ItemAdded
    tf 04 rmo CartItems ->> 03";

    /// Every sample in `merman`'s `fixtures/eventmodeling`, verbatim.
    const MERMAN_FIXTURES: [&str; 10] = [
        "eventmodeling\n\ntf 01 ui CartScreen\ntf 02 cmd AddItem\ntf 03 evt ItemAdded\ntf 04 cmd RemoveItem\ntf 05 evt ItemRemoved\n",
        "eventmodeling\n\ntf 01 ui CartScreen\ntf 02 cmd AddItem\ntf 03 evt ItemAdded\n",
        "eventmodeling\n\ntf 01 ui CartScreen\ntf 02 cmd AddItem\ntf 03 evt ItemAdded\ntf 04 rmo CartItems ->> 03\ntf 05 evt AccountingItemAdded\n",
        "eventmodeling\n\ntf 01 ui CartScreen\ntf 02 cmd AddItem { productId: 7 }\ntf 03 evt ItemAdded [[ItemAddedData]]\n\ndata ItemAddedData\n{\n  productId: 7,\n  quantity: 1\n}\n",
        "eventmodeling\n\ntf 01 ui CartScreen\ntf 02 cmd AddItem\ntf 03 cmd RemoveItem\ntf 04 evt ItemChanged ->> 02 ->> 03\n",
        "eventmodeling\n\ntf 01 ui CartScreen\ntf 02 cmd Inventory.AddItem\ntf 03 evt Inventory.ItemAdded\n",
        "eventmodeling\ntf 01 ui Shop.Cart\ntf 02 cmd Ordering.AddItem ->> 01 { sku: \"SKU-1\" }\ntf 03 evt Cart.ItemAdded ->> 02 [[ItemAddedData]]\nrf 04 rmo Read.CartSummary\ntf 05 evt Checkout.CheckedOut\n\ndata ItemAddedData {\n  sku: \"SKU-1\"\n  quantity: 1\n}\n",
        "eventmodeling\ntimeframe 01 event Start\n",
        "eventmodeling\n\ntimeframe 02 ui UI\ntf 01 evt Product.PriceChanged\ntf 03 evt Cart.ItemAdded\n",
        "eventmodeling\n\ntf 02 ui UI\nresetframe 01 evt Product.PriceChanged\ntf 03 evt Cart.ItemAdded\n",
    ];

    /// The samples in `merman`'s own parser tests.
    const MERMAN_PARSER_SAMPLES: [&str; 3] = [
        "eventmodeling\ntimeframe 01 event Start\n",
        "eventmodeling\ntf 02 ui UI\nresetframe 01 evt Product.PriceChanged\ntf 03 evt Cart.ItemAdded ->> 01 ->> 02\n",
        "eventmodeling\ntf 01 cmd AddItem { productId: 7 }\ntf 02 evt ItemAdded [[ItemAddedData]]\n\ndata ItemAddedData {\n  productId: 7\n}\n",
    ];

    fn parse(source: &str) -> EventModelingDocument {
        parse_event_modeling(source)
            .unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    fn refusal(source: &str) -> String {
        match parse_event_modeling(source) {
            Ok(document) => panic!("{source} should be refused, got {document:?}"),
            Err(error) => format!("{error}"),
        }
    }

    /// Writing back what was read must give text that reads the same and writes
    /// the same, otherwise confirming twice keeps rewriting the fence.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_event_modeling(&first);
        let second = parse_event_modeling(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        assert_eq!(first, second, "the document changed through:\n{first_text}");
        let second_text = emit_event_modeling(&second);
        assert_eq!(first_text, second_text, "emitting twice changed the source");
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "source rendered an error diagram:\n{source}"
        );
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        let source = "eventmodeling\n    tf 01 ui Screen";
        assert!(
            is_event_modeling_source(source),
            "{source} should be an event model"
        );
        for source in [
            "eventmodelings",
            "eventModeling",
            "EventModeling",
            "",
            "%% event_modeling",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_event_modeling_source(source),
                "{source} should not be an event model"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(emit_event_modeling(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_eq!(document.frames.len(), 4);
        let last = &document.frames[3];
        assert_eq!(last.entity_type, EventModelingEntityType::ReadModel);
        assert_eq!(last.label, "CartItems");
        assert_eq!(last.sources, ["03"]);
        assert_eq!(document.frames[0].leading_lines.len(), 1);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_merman_sample_opens_round_trips_and_renders() {
        for source in MERMAN_FIXTURES.iter().chain(&MERMAN_PARSER_SAMPLES) {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
            // What is drawn must not change: the same frames, arrows and data.
            let before = parse(source);
            let after = parse(&emitted);
            assert_eq!(before.frames, after.frames, "{emitted}");
            assert_eq!(before.data_blocks, after.data_blocks, "{emitted}");
            assert_eq!(before.structure_error(), None, "{source}");
        }
    }

    #[test]
    fn a_sample_keeps_its_spelling_arrows_and_data() {
        let document = parse(MERMAN_FIXTURES[6]);
        let cart = &document.frames[0];
        assert_eq!(cart.namespace.as_deref(), Some("Shop"));
        assert_eq!(cart.label, "Cart");
        let add = &document.frames[1];
        assert_eq!(add.sources, ["01"]);
        assert_eq!(add.inline_data.as_deref(), Some("{ sku: \"SKU-1\" }"));
        assert_eq!(
            document.frames[2].data_reference.as_deref(),
            Some("ItemAddedData")
        );
        assert!(document.frames[3].reset);
        assert_eq!(
            document.data_blocks[0].value,
            "{\n  sku: \"SKU-1\"\n  quantity: 1\n}"
        );
        assert_eq!(
            emit_event_modeling(&document),
            "eventmodeling
    tf 01 ui Shop.Cart
    tf 02 cmd Ordering.AddItem ->> 01 { sku: \"SKU-1\" }
    tf 03 evt Cart.ItemAdded ->> 02 [[ItemAddedData]]
    rf 04 rmo Read.CartSummary
    tf 05 evt Checkout.CheckedOut
    data ItemAddedData {
  sku: \"SKU-1\"
  quantity: 1
}"
        );

        let long = parse("eventmodeling\ntimeframe 01 event Start\nresetframe 02 command Go");
        assert_eq!(
            emit_event_modeling(&long),
            "eventmodeling\n    timeframe 01 event Start\n    resetframe 02 command Go"
        );
    }

    /// `merman` 0.7 read a one-line data block on the line after its name,
    /// and the model wrote one there; 0.8 refuses a block on a single line
    /// wherever it stands, as Mermaid does, so the model refuses to write one.
    #[test]
    fn a_data_block_is_written_over_several_lines() {
        let mut document = parse(TEMPLATE);
        document.frames[2].data_reference = Some("Item".to_string());
        document.data_blocks.push(EventModelingDataBlock {
            name: "Item".to_string(),
            value: "{ id: 1 }".to_string(),
            leading_lines: Vec::new(),
        });
        let error = document.structure_error().expect("a one-line block");
        assert!(error.contains("cannot be written on one line"), "{error}");
        assert!(render_to_svg(&emit_event_modeling(&document), &MermaidTheme::default()).is_err());

        if let Some(block) = document.data_blocks.first_mut() {
            block.value = "{\n  id: 1\n}".to_string();
        }
        let mut extra = document.frames[1].clone();
        extra.name = "05".to_string();
        document.frames.push(extra);
        assert_eq!(document.structure_error(), None);
        let emitted = emit_event_modeling(&document);
        assert!(
            emitted.ends_with("    data Item {\n  id: 1\n}"),
            "{emitted}"
        );
        assert_eq!(parse(&emitted), document);
        assert_renders(&emitted);

        // A `}` that is not at the start of its line does not close the block
        // for Mermaid.
        if let Some(block) = document.data_blocks.first_mut() {
            block.value = "{\n  id: 1\n  }".to_string();
        }
        assert!(document.structure_error().is_some());

        for source in [
            "eventmodeling\ndata A { id: 1 }\ntf 01 ui Screen [[A]]",
            "eventmodeling\ntf 01 ui Screen [[A]]\ndata A\n{ id: 1 }",
        ] {
            let refused = refusal(source);
            assert!(
                refused.contains("one-line block") || refused.contains("Mermaid could not parse"),
                "{refused}"
            );
        }
    }

    #[test]
    fn comments_stay_where_they_were() {
        let source = "---
title: 注文
---
%%{init: {\"theme\": \"dark\"}}%%
eventmodeling
    %% 画面
    tf 01 ui Screen
    %% 操作の前
    %% 操作
    tf 02 cmd Add
    %% データの前
    data D {
      a: 1
}
    %% 最後";
        let document = parse(source);
        assert_eq!(document.frames[0].leading_lines, ["    %% 画面"]);
        assert_eq!(
            document.frames[1].leading_lines,
            ["    %% 操作の前", "    %% 操作"]
        );
        assert_eq!(document.data_blocks[0].leading_lines, ["    %% データの前"]);
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);

        // A frame moved keeps the lines written in front of it.
        let mut moved = document;
        moved.frames.swap(0, 1);
        let emitted = emit_event_modeling(&moved);
        assert!(
            emitted.contains(
                "    %% 操作の前\n    %% 操作\n    tf 02 cmd Add\n    %% 画面\n    tf 01 ui Screen"
            ),
            "{emitted}"
        );

        // `merman` 0.7 skipped a `title` or accessibility line; 0.8 reads it,
        // but Mermaid refuses it in an event model, so the model does too,
        // and it refuses the statements it does not carry.
        for (source, expected) in [
            (
                "eventmodeling\n    title T\n    tf 01 ui A",
                "cannot be used in event modeling",
            ),
            (
                "eventmodeling\n    tf 01 ui A\n    accTitle: x",
                "cannot be used in event modeling",
            ),
            (
                "eventmodeling\n    accDescr {\n      x\n    }\n    tf 01 ui A",
                "cannot be used in event modeling",
            ),
            (
                "eventmodeling\n    tf 01 ui A\n    note 01\n{\n  x\n}",
                "`note`",
            ),
            ("eventmodeling\n    tfx 01 ui A", "Mermaid could not parse"),
        ] {
            let message = refusal(source);
            assert!(message.contains(expected), "{source}: {message}");
        }
    }

    /// A data block refused only for a comment on one of its lines says so,
    /// rather than telling the user its block is written on one line; a block
    /// that is on one line is still told that, comment or not.
    #[test]
    fn a_comment_on_a_data_line_is_named_as_the_reason() {
        for source in [
            "eventmodeling\ntf 01 ui A\ndata D {\n  a\n} %% note",
            "eventmodeling\ntf 01 ui A\ndata D %% note\n{\n  a\n}",
            "eventmodeling\ntf 01 ui A\ndata D { %% note\n  a\n}",
            "eventmodeling\ntf 01 ui A\ndata D {\n  a %% note\n}",
        ] {
            let message = refusal(source);
            assert!(
                message.contains("comment") && !message.contains("one-line"),
                "{source}: {message}"
            );
        }
        for source in [
            "eventmodeling\ntf 01 ui A\ndata D { a } %% note",
            "eventmodeling\ntf 01 ui A\ndata D { a }",
        ] {
            let message = refusal(source);
            assert!(message.contains("one-line"), "{source}: {message}");
        }
    }

    #[test]
    fn what_merman_would_read_differently_is_refused() {
        for (source, expected) in [
            ("eventmodeling\ntf 01 ui Cart Screen", "Mermaid cannot read"),
            ("eventmodeling\ntf 01 ui A %% note", "comment"),
            ("eventmodeling\ntf 01 box A", "does not know the type"),
            ("eventmodeling\ntf 01 ui", "has no name"),
            ("eventmodeling tf 01 ui A", "next line"),
            ("eventmodeling\ntf 零 evt Start", "frame number"),
            (
                "eventmodeling\ntf 01 cmd A { x } [[D]]\ndata D {\n  x\n}",
                "inline data and a data reference",
            ),
            ("eventmodeling\ntf 01 cmd A ->>", "->>"),
            ("eventmodeling\ntf 01 cmd A { x", "is not closed"),
            ("eventmodeling\ndata D {\n  %% x\n  a\n}", "comment line"),
            ("eventmodeling\ndata", "no data name"),
            ("eventmodeling\ndata D", "has no contents"),
        ] {
            let message = refusal(source);
            assert!(
                message.contains(expected),
                "{source}: expected {expected} in {message}"
            );
        }
        // `merman` 0.7 found arrows and references anywhere after the name,
        // inside the braces too, and split words at every single space; 0.8
        // reads inline data to the last `}` of the line and words as tokens,
        // as Mermaid does, so these open and come back as written.
        for source in [
            "eventmodeling\n    tf 01 cmd A { x ->> y }",
            "eventmodeling\n    tf 01 cmd A { [[D]] }",
        ] {
            assert_eq!(assert_stable(source), source);
            assert_renders(source);
        }
        assert_eq!(
            assert_stable("eventmodeling\ntf 01  ui   A"),
            "eventmodeling\n    tf 01 ui A"
        );
    }

    #[test]
    fn a_broken_structure_is_reported_before_it_is_written() {
        let base = parse(TEMPLATE);
        let data_block = |value: &str| EventModelingDataBlock {
            name: "D".to_string(),
            value: value.to_string(),
            leading_lines: Vec::new(),
        };
        let cases: Vec<(Box<dyn Fn(&mut EventModelingDocument)>, &str)> = vec![
            (
                Box::new(|document| document.frames[1].name = "01".to_string()),
                "used more than once",
            ),
            (
                Box::new(|document| document.frames[0].sources = vec!["03".to_string()]),
                "earlier frame",
            ),
            (
                Box::new(|document| document.frames[3].sources = vec!["04".to_string()]),
                "points to itself",
            ),
            (
                Box::new(|document| {
                    document.frames[3].sources = vec!["03".to_string(), "03".to_string()]
                }),
                "two arrows",
            ),
            (
                Box::new(|document| {
                    document.frames[2].data_reference = Some("Missing".to_string())
                }),
                "does not exist",
            ),
            (
                Box::new(|document| document.frames[0].label = "Cart Screen".to_string()),
                "The name `",
            ),
            // Mermaid reads names as ASCII identifiers.
            (
                Box::new(|document| document.frames[0].label = "かご画面".to_string()),
                "The name `",
            ),
            (
                Box::new(|document| document.frames[0].label = "1st".to_string()),
                "The name `",
            ),
            (
                Box::new(|document| document.frames[0].name = "0001".to_string()),
                "frame number",
            ),
            (
                Box::new(|document| document.frames[0].namespace = Some("店".to_string())),
                "The lane `",
            ),
            (
                Box::new(|document| document.frames[0].label = "a.b".to_string()),
                "The name `",
            ),
            (
                Box::new(|document| document.frames[0].name = "0{1".to_string()),
                "frame number",
            ),
            (
                Box::new(|document| document.frames[0].name = String::new()),
                "frame number",
            ),
            (
                Box::new(|document| document.frames[0].namespace = Some("a.b".to_string())),
                "The lane `",
            ),
            (
                Box::new(|document| {
                    document.frames[1].inline_data = Some("{ a } ->> 01".to_string())
                }),
                "The data `",
            ),
            (
                Box::new(|document| document.frames[2].name = "03,".to_string()),
                "frame number",
            ),
            (
                Box::new(|document| document.frames[2].name = ";03".to_string()),
                "frame number",
            ),
            (
                Box::new(|document| {
                    document.data_blocks.push(EventModelingDataBlock {
                        name: "D;".to_string(),
                        value: "{\n  a\n}".to_string(),
                        leading_lines: Vec::new(),
                    })
                }),
                "data name",
            ),
            (
                Box::new(move |document| {
                    document.frames[1].inline_data = Some("{ a }".to_string());
                    document.frames[1].data_reference = Some("D".to_string());
                    document.data_blocks.push(data_block("{\n  a\n}"));
                }),
                "inline data and a data reference",
            ),
            (
                Box::new(move |document| document.data_blocks.push(data_block("{\n  a\n} b"))),
                "contents of data",
            ),
            (
                Box::new(move |document| {
                    document.data_blocks.push(data_block("{\n  a\n}"));
                    document.data_blocks.push(data_block("{\n  b\n}"));
                }),
                "used more than once",
            ),
            (
                Box::new(move |document| {
                    document.data_blocks.push(EventModelingDataBlock {
                        name: "データ".to_string(),
                        ..data_block("{\n  a\n}")
                    })
                }),
                "data name",
            ),
        ];
        for (edit, expected) in cases {
            let mut document = base.clone();
            edit(&mut document);
            let error = document
                .structure_error()
                .unwrap_or_else(|| panic!("expected {expected}: {document:?}"));
            assert!(error.contains(expected), "expected {expected} in {error}");
        }
    }

    #[test]
    fn edited_documents_that_pass_the_checks_are_read_back_as_written() {
        let mut document = parse(TEMPLATE);
        document.frames[0].namespace = Some("Shop".to_string());
        document.frames[1].inline_data = Some("{ 商品: 1 }".to_string());
        document.frames[1].reset = true;
        document.frames[2].sources = vec!["01".to_string(), "02".to_string()];
        document.frames[2].data_reference = Some("Detail".to_string());
        document.frames[3].entity_type = EventModelingEntityType::Processor;
        document.data_blocks.push(EventModelingDataBlock {
            name: "Detail".to_string(),
            value: "{\n  商品: 1,\n  数: 2\n}".to_string(),
            leading_lines: Vec::new(),
        });
        assert_eq!(document.structure_error(), None);
        let emitted = emit_event_modeling(&document);
        let reread = parse(&emitted);
        assert_eq!(reread, document, "{emitted}");
        assert_renders(&emitted);

        // A tag opened in one frame's data and closed in a later one has
        // `merman` change the quotes of the data between them, which must not
        // keep the written diagram from opening again. (Under `merman` 0.7 the
        // tag could open in a box's name, which Mermaid reads only as an
        // ASCII identifier.)
        let mut document = parse(TEMPLATE);
        document.frames[0].inline_data = Some("{ <b }".to_string());
        document.frames[1].inline_data = Some("{ a=\"x\" }".to_string());
        document.frames[2].inline_data = Some("{ c> }".to_string());
        assert_eq!(document.structure_error(), None);
        let emitted = emit_event_modeling(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_renders(&emitted);
    }

    #[test]
    fn a_blank_document_writes_a_diagram_merman_draws() {
        let emitted = emit_event_modeling(&EventModelingDocument::empty());
        assert_eq!(emitted, "eventmodeling");
        assert_eq!(parse(&emitted), EventModelingDocument::empty());
        assert_renders(&emitted);
    }

    /// `merman` 0.7 read any word as a frame's number or name, Japanese
    /// included; 0.8 reads numbers of one to three digits and ASCII
    /// identifiers, as Mermaid does, so a Japanese name no longer opens, and
    /// only data carries Japanese text.
    #[test]
    fn multibyte_names_are_refused_and_multibyte_data_is_kept() {
        for source in [
            "eventmodeling\n  tf 零 evt Start",
            "eventmodeling\n  tf 01 evt 始",
            "eventmodeling\n  tf 01 ui 画面.Cart",
        ] {
            assert!(
                render_to_svg(source, &MermaidTheme::default()).is_err(),
                "{source}"
            );
            refusal(source);
        }
        let document =
            parse("eventmodeling\n  tf 01 evt Start\n  tf 02 ui Screen.Cart ->> 01 {あ}");
        assert_eq!(document.frames[1].sources, ["01"]);
        assert_eq!(document.frames[1].namespace.as_deref(), Some("Screen"));
        assert_eq!(document.frames[1].label, "Cart");
        assert_eq!(document.frames[1].inline_data.as_deref(), Some("{あ}"));
    }
}
