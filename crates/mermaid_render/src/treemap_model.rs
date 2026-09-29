//! A round-trippable editing model for Mermaid treemaps.
//!
//! The shape of this module mirrors [`crate::mindmap_model`]: [`parse_treemap`]
//! reads the body of a ```` ```mermaid ```` fence into [`TreemapDocument`], and
//! [`emit_treemap`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! A treemap is a forest spelled with indentation. An item with a value
//! (`"名前": 10`) is a leaf and one without is a section, and `merman` never
//! hangs anything under a leaf: an item indented deeper than a leaf goes to
//! the section the leaf is in. The model keeps the forest that reading
//! produces, one row per item with its depth, and the emitter writes every
//! level four spaces deeper than its parent, which reads back the same.
//!
//! What this module refuses rather than rewrites, all of it with the reason in
//! Japanese:
//!
//! - text after the `treemap` keyword, which `merman` rejects;
//! - a `%%` comment at the end of a line, which `merman` drops;
//! - a value `merman` reads as a different number (`1_000` is read as `1`);
//! - a second `title`, of which `merman` keeps only the last;
//! - text after the `;` of a `classDef`, which `merman` drops;
//! - anything `merman` reads differently from this module, which is checked by
//!   asking it after every parse.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the spacing around `:` and `:::` (`,` before a value becomes
//! `:`), the position of the `title` line (right after the keyword) and of the
//! `classDef` lines (after the items), and comment lines between statements,
//! which move below the items the way they do in a mindmap.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use serde_json::Value;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::kanban_model::{mermaid_rewrites, statement_after_directive};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};
use crate::mindmap_model::{OutlineItem, outline_parent, outline_subtree_end};

const HEADERS: &[&str] = &["treemap-beta", "treemap"];
const DEFAULT_HEADER: &str = "treemap-beta";

/// One box of the treemap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreemapItem {
    pub name: String,
    /// The quote the name is written between, `"` or `'`.
    pub quote: char,
    /// The number as the source spells it (`1,000.50`). An item with a value
    /// is a leaf; one without is a section that holds the items below it.
    pub value: Option<String>,
    /// The class named after `:::`.
    pub class: Option<String>,
}

impl TreemapItem {
    pub fn section(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            quote: '"',
            value: None,
            class: None,
        }
    }

    pub fn leaf(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
            ..Self::section(name)
        }
    }

    /// Switches to the other quote when the name holds the one it has, so that
    /// a name with either quote in it can still be written.
    pub fn fit_quote(&mut self) {
        let other = if self.quote == '"' { '\'' } else { '"' };
        if self.name.contains(self.quote) && !self.name.contains(other) {
            self.quote = other;
        }
    }
}

/// One item of the forest in the order the fence writes them, with its depth
/// instead of its children, which is the shape the form edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreemapEntry {
    /// 0 for a top-level item, 1 for the items in it and so on.
    pub depth: usize,
    pub item: TreemapItem,
}

impl OutlineItem for TreemapEntry {
    fn depth(&self) -> usize {
        self.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }
}

/// A `classDef` line: a class name and the styles it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreemapClassDef {
    pub name: String,
    /// The styles before the `;`, e.g. `fill:#f96,stroke:#333`.
    pub style: String,
    /// Whether the line ends with `;`.
    pub terminated: bool,
}

/// An editable treemap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreemapDocument {
    /// `treemap-beta` or `treemap`, as the source spells it.
    pub header: &'static str,
    pub title: Option<String>,
    pub entries: Vec<TreemapEntry>,
    pub class_defs: Vec<TreemapClassDef>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl TreemapDocument {
    /// A blank treemap, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            header: DEFAULT_HEADER,
            title: None,
            entries: Vec::new(),
            class_defs: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_treemap_title(title)
        {
            return Some(i18n::t!(
                "The title `{title}` cannot be written (line breaks, `%%`, `=\"` and `#…;` are not allowed)."
            ));
        }
        let mut previous_depth: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(problem) = treemap_item_problem(&entry.item) {
                return Some(problem);
            }
            let deepest = previous_depth.map_or(0, |depth| depth + 1);
            if entry.depth > deepest {
                return Some(i18n::t!(
                    "The item `{}` is two or more levels deeper than its parent.",
                    entry.item.name
                ));
            }
            if entry.item.value.is_some()
                && self
                    .entries
                    .get(index + 1)
                    .is_some_and(|next| next.depth > entry.depth)
            {
                return Some(i18n::t!(
                    "Items cannot be placed under the item `{}`, which has a value. To put items inside it, leave its value empty.",
                    entry.item.name
                ));
            }
            previous_depth = Some(entry.depth);
        }
        for class_def in &self.class_defs {
            if !is_valid_treemap_class_name(&class_def.name) {
                return Some(i18n::t!(
                    "The class name `{}` cannot be used (only ASCII letters, digits and `_`, starting with a letter or `_`).",
                    class_def.name
                ));
            }
            if !is_valid_treemap_style(&class_def.style) {
                return Some(i18n::t!(
                    "The style `{style}` of the class `{name}` cannot be written (separate `name:value` pairs with `,`; `;` and `%%` are not allowed).",
                    name = class_def.name,
                    style = class_def.style
                ));
            }
        }
        None
    }
}

/// Adds `entry` right below the item at `index` and its subtree, on the same
/// level, returning where it went. Unlike a mindmap's root, a top-level item
/// can have siblings.
pub fn treemap_add_sibling<T: OutlineItem>(
    entries: &mut Vec<T>,
    index: usize,
    mut entry: T,
) -> Option<usize> {
    entry.set_depth(entries.get(index)?.depth());
    let at = outline_subtree_end(entries, index);
    entries.insert(at, entry);
    Some(at)
}

/// Removes the item at `index` together with everything in it.
pub fn treemap_remove<T: OutlineItem>(entries: &mut Vec<T>, index: usize) -> Option<Vec<T>> {
    entries.get(index)?;
    let end = outline_subtree_end(entries, index);
    Some(entries.drain(index..end).collect())
}

/// Makes the item at `index` the sibling right after its parent, returning
/// where it went. The siblings below it stay with the parent.
pub fn treemap_outdent<T: OutlineItem>(entries: &mut Vec<T>, index: usize) -> Option<usize> {
    if entries.get(index)?.depth() == 0 {
        return None;
    }
    let parent = outline_parent(entries, index)?;
    let end = outline_subtree_end(entries, index);
    let mut subtree = entries.drain(index..end).collect::<Vec<_>>();
    for entry in &mut subtree {
        entry.set_depth(entry.depth().saturating_sub(1));
    }
    let at = outline_subtree_end(entries, parent);
    let tail = entries.split_off(at);
    entries.extend(subtree);
    entries.extend(tail);
    Some(at)
}

/// Whether `source` is a treemap: its first statement is `treemap-beta` or `treemap`.
pub fn is_treemap_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_treemap(source: &str) -> Result<TreemapDocument> {
    let document = scan_treemap(source)?;
    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        check_against_merman(source, &document)?;
    }
    Ok(document)
}

fn scan_treemap(source: &str) -> Result<TreemapDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(TreemapDocument {
            preserved_lines,
            ..TreemapDocument::empty()
        });
    };
    let header_line = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some((header, tail)) = HEADERS
        .iter()
        .find_map(|header| keyword_tail(header_line, header).map(|tail| (*header, tail)))
    else {
        bail!(i18n::t!(
            "This is not a treemap (only a diagram that starts with `treemap-beta` can be edited in the structured editor)."
        ));
    };
    if !tail.is_empty() {
        bail!(i18n::t!(
            "The structured editor does not support `{tail}` on the `{header}` line (Mermaid cannot draw this diagram)."
        ));
    }

    let mut title: Option<String> = None;
    let mut rows: Vec<(usize, TreemapItem)> = Vec::new();
    let mut class_defs = Vec::new();
    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
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
            let text = join_lines(lines.get(start..index).unwrap_or_default());
            if let Some(statement) = statement_after_directive(&text) {
                bail!(i18n::t!(
                    "`{statement}` after `%%{{…}}%%` is not supported by the structured editor. Put the directive and the statement on separate lines, then open it again."
                ));
            }
            preserved_lines.push(PreservedLine { position, text });
            continue;
        }
        let statement = strip_inline_comment(line);
        if statement.trim_end().len() != line.trim_end().len() {
            bail!(i18n::t!(
                "The structured editor would lose the comment at the end of the statement ({statement}). Move the comment to a line of its own, or edit it as text.",
                statement = trimmed
            ));
        }
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            preserved_lines.push(PreservedLine {
                position,
                text: line.trim_end().to_string(),
            });
            continue;
        }
        if trimmed.starts_with("title") {
            let Some(text) = keyword_tail(trimmed, "title") else {
                bail!(i18n::t!(
                    "The structured editor does not support `{trimmed}`. Put a space after `title`."
                ));
            };
            if title.is_some() {
                bail!(i18n::t!(
                    "`title` is written twice. Mermaid uses only the later one, so merge them into one, then open the editor again."
                ));
            }
            title = (!text.is_empty()).then(|| text.to_string());
            continue;
        }
        // The title is written right after the keyword, above the comments
        // that came before the first item, so it does not end them.
        position = PreservedPosition::Bottom;
        if trimmed.starts_with("classDef") {
            class_defs.push(read_class_def(trimmed)?);
            continue;
        }

        let rest = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - rest.len();
        rows.push((indent, read_item(rest.trim_end())?));
    }

    // A leaf is never pushed: an item indented deeper than a leaf goes to the
    // section the leaf is in, which is what `merman`'s stack does too.
    let mut open_sections: Vec<usize> = Vec::new();
    let mut entries = Vec::with_capacity(rows.len());
    for (indent, item) in rows {
        while open_sections.last().is_some_and(|open| *open >= indent) {
            open_sections.pop();
        }
        let is_section = item.value.is_none();
        entries.push(TreemapEntry {
            depth: open_sections.len(),
            item,
        });
        if is_section {
            open_sections.push(indent);
        }
    }

    Ok(TreemapDocument {
        header,
        title,
        entries,
        class_defs,
        preserved_lines,
    })
}

fn read_class_def(statement: &str) -> Result<TreemapClassDef> {
    let unsupported = || {
        anyhow!(i18n::t!(
            "The structured editor does not support `{statement}` (only the form `classDef name style;`). Edit it as text."
        ))
    };
    let rest = keyword_tail(statement, "classDef").ok_or_else(unsupported)?;
    let name_length = class_name_length(rest);
    let name = rest.get(..name_length).unwrap_or_default();
    let after = rest.get(name_length..).unwrap_or_default();
    if name.is_empty() || !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return Err(unsupported());
    }
    let after = after.trim();
    let (style, terminated) = match after.split_once(';') {
        Some((style, dropped)) => {
            if !dropped.trim().is_empty() {
                bail!(i18n::t!(
                    "Mermaid skips what follows the `;` in `{statement}`. Delete it or edit it as text."
                ));
            }
            (style.trim(), true)
        }
        None => (after, false),
    };
    Ok(TreemapClassDef {
        name: name.to_string(),
        style: style.to_string(),
        terminated,
    })
}

/// Reads an item line, with its indentation removed, the way `merman`'s
/// `parse_item_row` does.
fn read_item(rest: &str) -> Result<TreemapItem> {
    let unreadable = || {
        anyhow!(i18n::t!(
            "This could not be read as a treemap item ({rest}). Write it as `\"name\"` or `\"name\": value`."
        ))
    };
    let quote = rest
        .chars()
        .next()
        .filter(|character| matches!(character, '"' | '\''))
        .ok_or_else(unreadable)?;
    let body = rest.get(1..).unwrap_or_default();
    let close = body.find(quote).ok_or_else(unreadable)?;
    let name = body.get(..close).unwrap_or_default().to_string();
    let mut after = body.get(close + 1..).unwrap_or_default().trim_start();

    let mut value = None;
    if !after.starts_with(":::")
        && let Some(number) = after.strip_prefix([':', ','])
    {
        let number = number.trim_start();
        let length = number
            .find(|character: char| {
                !(character.is_ascii_digit() || matches!(character, '_' | '.' | ','))
            })
            .unwrap_or(number.len());
        let token = number.get(..length).unwrap_or_default();
        if token.is_empty() {
            return Err(unreadable());
        }
        if !is_valid_treemap_value(token) {
            bail!(i18n::t!(
                "The structured editor does not support the value `{token}` of the item `{name}` (Mermaid skips everything from a `_` or a second `.` on)."
            ));
        }
        value = Some(token.to_string());
        after = number.get(length..).unwrap_or_default().trim_start();
    }

    let class = if after.is_empty() {
        None
    } else {
        let Some(class_part) = after.strip_prefix(":::") else {
            return Err(unreadable());
        };
        let class_part = class_part.trim_start();
        let length = class_name_length(class_part);
        let class = class_part.get(..length).unwrap_or_default();
        if class.is_empty()
            || !class_part
                .get(length..)
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            return Err(unreadable());
        }
        Some(class.to_string())
    };
    Ok(TreemapItem {
        name,
        quote,
        value,
        class,
    })
}

/// The length of the class name at the start of `text`, the way `merman`'s
/// `parse_id2` reads one: a letter or `_`, then letters, digits and `_`.
fn class_name_length(text: &str) -> usize {
    let mut characters = text.char_indices();
    match characters.next() {
        Some((_, first)) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return 0,
    }
    characters
        .find(|(_, character)| !(character.is_ascii_alphanumeric() || *character == '_'))
        .map_or(text.len(), |(offset, _)| offset)
}

/// Cuts `line` at the first `%%` outside a `"` or `'` string, the way
/// `merman` does before it reads a treemap statement.
fn strip_inline_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut characters = line.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            continue;
        }
        if character == '%' && characters.peek().is_some_and(|(_, next)| *next == '%') {
            return line.get(..offset).unwrap_or_default();
        }
    }
    line
}

/// The number `merman` reads out of a value token: the commas dropped, then
/// the digits with at most one `.`, an integer when nothing but zeros follows
/// the point.
pub fn treemap_number(token: &str) -> Option<Value> {
    let without_commas = token
        .chars()
        .filter(|character| *character != ',')
        .collect::<String>();
    let mut saw_dot = false;
    let mut cut = 0;
    for character in without_commas.chars() {
        if character.is_ascii_digit() {
            cut += 1;
        } else if character == '.' && !saw_dot {
            saw_dot = true;
            cut += 1;
        } else {
            break;
        }
    }
    let prefix = without_commas.get(..cut)?;
    if prefix.is_empty() {
        return None;
    }
    if saw_dot {
        let (integer, fraction) = prefix.split_once('.').unwrap_or((prefix, ""));
        if fraction.chars().all(|character| character == '0') {
            return integer.parse::<i64>().ok().map(Value::from);
        }
        let number = serde_json::Number::from_f64(prefix.parse::<f64>().ok()?)?;
        return Some(Value::Number(number));
    }
    prefix.parse::<i64>().ok().map(Value::from)
}

/// Compares what `merman` reads out of `source` with what the model does, so
/// that a treemap the two read differently is refused instead of rewritten.
fn check_against_merman(source: &str, document: &TreemapDocument) -> Result<()> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(i18n::t!(
                "Mermaid could not parse this diagram ({error}). Edit it as text."
            ))
        })?
        .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
    let RenderSemanticModel::Treemap(model) = parsed.into_parts().1 else {
        bail!(i18n::t!("This diagram is not a treemap."));
    };

    let drawn_title = model
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let title_matches = match (drawn_title, document.title.as_deref()) {
        (Some(drawn), Some(expected)) => drawn == expected || expected.contains(['<', '&']),
        (None, None) => true,
        _ => false,
    };
    if !title_matches {
        bail!(i18n::t!(
            "The title could not be read the same way Mermaid reads it. Edit it as text."
        ));
    }

    // The tree in the order it is written, walked with a stack of its own so
    // that a deep treemap cannot run the thread out of stack.
    let mut drawn = Vec::new();
    let mut pending = model
        .root
        .children
        .iter()
        .flatten()
        .rev()
        .map(|node| (0, node))
        .collect::<Vec<_>>();
    while let Some((depth, node)) = pending.pop() {
        drawn.push((depth, node));
        pending.extend(
            node.children
                .iter()
                .flatten()
                .rev()
                .map(|child| (depth + 1, child)),
        );
    }

    if drawn.len() != document.entries.len() {
        bail!(i18n::t!(
            "Mermaid read this treemap as {} items, but the structured editor read {}. Edit it as text.",
            drawn.len(),
            document.entries.len()
        ));
    }
    for ((depth, node), entry) in drawn.iter().zip(&document.entries) {
        let item = &entry.item;
        let matches = *depth == entry.depth
            && node.name == item.name
            && node.children.is_none() == item.value.is_some()
            && node.value == item.value.as_deref().and_then(treemap_number)
            && node.class_selector == item.class;
        if !matches {
            bail!(i18n::t!(
                "The item `{}` could not be read the same way Mermaid reads it. Edit it as text.",
                item.name
            ));
        }
    }
    Ok(())
}

fn emit_item(item: &TreemapItem) -> String {
    let mut line = format!("{0}{1}{0}", item.quote, item.name);
    if let Some(value) = &item.value {
        line.push_str(": ");
        line.push_str(value);
    }
    if let Some(class) = &item.class {
        line.push_str(":::");
        line.push_str(class);
    }
    line
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_treemap(document: &TreemapDocument) -> String {
    let mut out = String::new();
    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(document.header);
    out.push('\n');
    if let Some(title) = &document.title {
        out.push_str(INDENT);
        out.push_str("title ");
        out.push_str(title);
        out.push('\n');
    }
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    for entry in &document.entries {
        out.push_str(&INDENT.repeat(entry.depth + 1));
        out.push_str(&emit_item(&entry.item));
        out.push('\n');
    }
    for class_def in &document.class_defs {
        out.push_str(INDENT);
        out.push_str("classDef ");
        out.push_str(&class_def.name);
        if !class_def.style.is_empty() {
            out.push(' ');
            out.push_str(&class_def.style);
        }
        if class_def.terminated {
            out.push(';');
        }
        out.push('\n');
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
    document: &TreemapDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `title` can follow `title` on its line.
pub fn is_valid_treemap_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.trim() == title
        && !title.contains(['\n', '\r'])
        && !title.contains("%%")
        && !mermaid_rewrites(title)
}

/// Whether the item's name can be written between its quotes.
pub fn is_valid_treemap_name(item: &TreemapItem) -> bool {
    !item.name.trim().is_empty()
        && !item.name.contains(['\n', '\r'])
        && !item.name.contains(item.quote)
        && !mermaid_rewrites(&item.name)
}

/// Whether `value` is a number `merman` reads the way it is written: digits,
/// an optional `.` and its digits, with `,` allowed as a thousands separator.
pub fn is_valid_treemap_value(value: &str) -> bool {
    let without_commas = value
        .chars()
        .filter(|character| *character != ',')
        .collect::<String>();
    let (integer, fraction) = without_commas
        .split_once('.')
        .unwrap_or((without_commas.as_str(), ""));
    value.starts_with(|character: char| character.is_ascii_digit())
        && !integer.is_empty()
        && integer.chars().all(|character| character.is_ascii_digit())
        && fraction.chars().all(|character| character.is_ascii_digit())
        && treemap_number(value).is_some()
}

/// Whether `name` can follow `:::` or `classDef`.
pub fn is_valid_treemap_class_name(name: &str) -> bool {
    !name.is_empty() && class_name_length(name) == name.len()
}

/// Whether `style` can follow a class name on a `classDef` line: `name:value`
/// pairs separated by `,` (a `\,` stays inside a value), which is what
/// `merman` checks before it draws anything.
pub fn is_valid_treemap_style(style: &str) -> bool {
    if style.contains([';', '\n', '\r']) || style.contains("%%") || style.trim() != style {
        return false;
    }
    let replaced = style.replace("\\,", "\u{0}");
    replaced.split(',').all(|token| {
        let token = token.trim();
        token.is_empty()
            || token
                .split_once(':')
                .is_some_and(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty())
    })
}

/// The first thing wrong with one item, in Japanese.
pub fn treemap_item_problem(item: &TreemapItem) -> Option<String> {
    if item.name.trim().is_empty() {
        return Some(i18n::t!("There is an item with an empty name.").to_string());
    }
    if !is_valid_treemap_name(item) {
        return Some(i18n::t!(
            "The item name `{}` cannot be written (line breaks, `%%{{`, `=\"` and `#…;` are not allowed, and it cannot contain both `\"` and `'`).",
            item.name
        ));
    }
    if let Some(value) = &item.value
        && !is_valid_treemap_value(value)
    {
        return Some(i18n::t!(
            "The value `{value}` of the item `{}` cannot be read as a number (a number of 0 or more; `,` as a thousands separator and `.` as the decimal point are allowed).",
            item.name
        ));
    }
    if let Some(class) = &item.class
        && !is_valid_treemap_class_name(class)
    {
        return Some(i18n::t!(
            "The class `{class}` of the item `{}` cannot be used.",
            item.name
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mindmap_model::{outline_add_child, outline_indent, outline_move};
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    const TEMPLATE: &str = "treemap-beta\n    %% 字下げで入れ子。末端の項目に : で値を付ける\n    \"開発費\"\n        \"設計\": 30\n        \"実装\": 50\n    \"運用費\"\n        \"保守\": 15\n        \"監視\": 5";

    fn parse(source: &str) -> TreemapDocument {
        parse_treemap(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    fn refusal(source: &str) -> String {
        match parse_treemap(source) {
            Ok(document) => panic!("{source} should be refused, got {document:?}"),
            Err(error) => error.to_string(),
        }
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_treemap(&first);
        let second = parse(&first_text);
        let second_text = emit_treemap(&second);
        assert_eq!(
            first, second,
            "the document changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    fn outline(document: &TreemapDocument) -> Vec<(usize, String, Option<String>)> {
        document
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.depth,
                    entry.item.name.clone(),
                    entry.item.value.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(
            outline(&document),
            vec![
                (0, "開発費".to_string(), None),
                (1, "設計".to_string(), Some("30".to_string())),
                (1, "実装".to_string(), Some("50".to_string())),
                (0, "運用費".to_string(), None),
                (1, "保守".to_string(), Some("15".to_string())),
                (1, "監視".to_string(), Some("5".to_string())),
            ]
        );
        assert_eq!(document.structure_error(), None);
        assert_eq!(emit_treemap(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn the_depth_is_read_the_way_merman_reads_it() {
        // An item indented deeper than a leaf goes to the leaf's section.
        let source =
            "treemap\n\"A\"\n    \"a1\": 1\n        \"a2\": 2\n  \"B\"\n\t\t\t\"b1\": 3\n\"C\"";
        let document = parse(source);
        assert_eq!(
            outline(&document),
            vec![
                (0, "A".to_string(), None),
                (1, "a1".to_string(), Some("1".to_string())),
                (1, "a2".to_string(), Some("2".to_string())),
                (1, "B".to_string(), None),
                (2, "b1".to_string(), Some("3".to_string())),
                (0, "C".to_string(), None),
            ]
        );
        assert_eq!(
            assert_stable(source),
            "treemap\n    \"A\"\n        \"a1\": 1\n        \"a2\": 2\n        \"B\"\n            \"b1\": 3\n    \"C\""
        );
    }

    #[test]
    fn titles_classes_values_and_comments_survive() {
        let source = "---\nconfig:\n  treemap:\n    valueFormat: \"$,.0f\"\n---\ntreemap-beta\n  %% 先頭のメモ\ntitle 予算 (千円)\naccTitle: 予算\n\"Main\":::important\n    \"A\", 1,234.50\n    %% 途中のメモ\n    'B \"引用\"': 1_0.0:::small_1\nclassDef important fill:#f96,stroke:#333,stroke-width:2px;\nclassDef small_1 color:red";
        let refused = refusal(source);
        assert!(refused.contains("`1_0.0`"), "{refused}");

        let source = source.replace("1_0.0", "10.0");
        let document = parse(&source);
        assert_eq!(document.title.as_deref(), Some("予算 (千円)"));
        assert_eq!(document.header, "treemap-beta");
        let classes = document
            .entries
            .iter()
            .map(|entry| entry.item.class.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            classes,
            vec![
                Some("important".to_string()),
                None,
                Some("small_1".to_string())
            ]
        );
        assert_eq!(document.class_defs.len(), 2);
        let emitted = assert_stable(&source);
        assert_eq!(
            emitted,
            "---\nconfig:\n  treemap:\n    valueFormat: \"$,.0f\"\n---\ntreemap-beta\n    title 予算 (千円)\n  %% 先頭のメモ\naccTitle: 予算\n    \"Main\":::important\n        \"A\": 1,234.50\n        'B \"引用\"': 10.0:::small_1\n    classDef important fill:#f96,stroke:#333,stroke-width:2px;\n    classDef small_1 color:red\n    %% 途中のメモ"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn what_mermaid_would_drop_or_misread_is_refused() {
        for (source, reason) in [
            (
                "treemap-beta utilities\n\"A\": 1",
                "on the `treemap-beta` line",
            ),
            ("treemap-beta\n\"A\": 1 %% メモ", "comment at the end"),
            ("treemap-beta\n\"A\": 1_000", "`1_000`"),
            ("treemap-beta\n\"A\": 1.2.3", "`1.2.3`"),
            ("treemap-beta\ntitle A\ntitle B\n\"A\": 1", "written twice"),
            (
                "treemap-beta\ntitleA\n\"A\": 1",
                "Put a space after `title`",
            ),
            (
                "treemap-beta\nclassDef a fill:red; x\n\"A\": 1",
                "follows the `;`",
            ),
            ("treemap-beta\nclassDef 日本 fill:red\n\"A\": 1", "classDef"),
            ("treemap-beta\nA: 1", "could not be read as a treemap item"),
            (
                "treemap-beta\n\"A\": 1 2",
                "could not be read as a treemap item",
            ),
            (
                "treemap-beta\n\"A\":::a-b",
                "could not be read as a treemap item",
            ),
            (
                "treemap-beta\n\"A\": x",
                "could not be read as a treemap item",
            ),
            (
                "treemap-beta\nclassDef c color\n\"A\": 1",
                "Mermaid could not parse",
            ),
        ] {
            let message = refusal(source);
            assert!(
                message.contains(reason),
                "{source:?} should be refused for {reason:?}, got {message}"
            );
        }
    }

    #[test]
    fn the_structure_check_catches_what_cannot_be_written() {
        let cases: Vec<(fn(&mut TreemapDocument), &str)> = vec![
            (
                |document| {
                    if let Some(entry) = document.entries.get_mut(1) {
                        entry.item.value = Some("1_000".to_string());
                    }
                },
                "The value `1_000`",
            ),
            (
                |document| {
                    if let Some(entry) = document.entries.get_mut(1) {
                        entry.item.name = "\"と'".to_string();
                    }
                },
                "contain both",
            ),
            (
                |document| {
                    if let Some(entry) = document.entries.get_mut(1) {
                        entry.item.name = " ".to_string();
                    }
                },
                "empty name",
            ),
            (
                |document| {
                    // 実装 goes under 設計, which has a value.
                    outline_indent(&mut document.entries, 2);
                },
                "under the item `設計`, which has a value",
            ),
            (
                |document| {
                    if let Some(entry) = document.entries.get_mut(1) {
                        entry.item.class = Some("a-b".to_string());
                    }
                },
                "class `a-b`",
            ),
            (
                |document| document.title = Some("a %% b".to_string()),
                "title",
            ),
            (
                |document| {
                    document.class_defs.push(TreemapClassDef {
                        name: "hot".to_string(),
                        style: "color".to_string(),
                        terminated: false,
                    })
                },
                "style `color`",
            ),
        ];
        for (edit, reason) in cases {
            let mut document = parse(TEMPLATE);
            edit(&mut document);
            let error = document.structure_error();
            assert!(
                error.as_deref().is_some_and(|error| error.contains(reason)),
                "expected {reason:?}, got {error:?}"
            );
        }

        let mut item = TreemapItem::section("a \"b\"");
        assert!(!is_valid_treemap_name(&item));
        item.fit_quote();
        assert_eq!(item.quote, '\'');
        assert!(is_valid_treemap_name(&item));
    }

    #[test]
    fn a_forest_can_be_rearranged() {
        let mut document = parse(TEMPLATE);
        // 運用費 goes first, taking its leaves with it.
        assert_eq!(outline_move(&mut document.entries, 3, false), Some(0));
        // A new section at the top level, a leaf in it, and 監視 outdented.
        let added = treemap_add_sibling(
            &mut document.entries,
            0,
            TreemapEntry {
                depth: 0,
                item: TreemapItem::section("予備費"),
            },
        );
        assert_eq!(added, Some(3));
        outline_add_child(
            &mut document.entries,
            3,
            TreemapEntry {
                depth: 0,
                item: TreemapItem::leaf("雑費", "1,000"),
            },
        );
        assert_eq!(treemap_outdent(&mut document.entries, 2), Some(2));
        assert_eq!(
            outline(&document),
            vec![
                (0, "運用費".to_string(), None),
                (1, "保守".to_string(), Some("15".to_string())),
                (0, "監視".to_string(), Some("5".to_string())),
                (0, "予備費".to_string(), None),
                (1, "雑費".to_string(), Some("1,000".to_string())),
                (0, "開発費".to_string(), None),
                (1, "設計".to_string(), Some("30".to_string())),
                (1, "実装".to_string(), Some("50".to_string())),
            ]
        );
        assert_eq!(
            treemap_remove(&mut document.entries, 5).map(|removed| removed.len()),
            Some(3)
        );
        assert_eq!(document.structure_error(), None);
        let emitted = emit_treemap(&document);
        assert_eq!(assert_stable(&emitted), emitted);
        assert_eq!(parse(&emitted), document);
        assert_renders(&emitted);
    }

    #[test]
    fn a_statement_after_a_directive_is_refused_rather_than_moved() {
        let message = refusal(
            "treemap-beta\n\"A\"\n    %%{init: {}}%% classDef c fill:red\n    \"b\": 1:::c",
        );
        assert!(
            message.contains("Put the directive and the statement on separate lines"),
            "{message}"
        );
        let source = "treemap-beta\n    %%{init: {}}%%\n    \"A\"\n        \"b\": 1";
        assert_eq!(assert_stable(source), source);
    }

    #[test]
    fn names_mermaid_rewrites_before_parsing_cannot_be_written() {
        // Mermaid turns `="1"` into `='1'` inside a tag, which would close a
        // name quoted with `'`.
        for name in ["<i x=\"1\">", "a %%{ b", "a#35;b"] {
            let mut item = TreemapItem::section(name);
            item.fit_quote();
            assert!(!is_valid_treemap_name(&item), "{name:?}");
            let document = TreemapDocument {
                entries: vec![TreemapEntry { depth: 0, item }],
                ..TreemapDocument::empty()
            };
            assert!(
                document
                    .structure_error()
                    .is_some_and(|error| error.contains("item name")),
                "{name:?}"
            );
        }
        assert!(!is_valid_treemap_title("a#35;b"));
        let mut item = TreemapItem::leaf("Issue #12 = \"x\"", "1");
        item.fit_quote();
        assert!(is_valid_treemap_name(&item));
        let source = emit_treemap(&TreemapDocument {
            entries: vec![TreemapEntry { depth: 0, item }],
            ..TreemapDocument::empty()
        });
        assert_eq!(assert_stable(&source), source);
        assert_renders(&source);
    }

    #[test]
    fn a_blank_treemap_writes_only_the_keyword() {
        let document = TreemapDocument::empty();
        assert_eq!(emit_treemap(&document), "treemap-beta");
        assert_eq!(parse("treemap-beta"), document);
        let emitted = emit_treemap(&TreemapDocument {
            entries: vec![TreemapEntry {
                depth: 0,
                item: TreemapItem::section("空の区分"),
            }],
            ..TreemapDocument::empty()
        });
        assert_renders(&emitted);
    }

    /// Everything `merman` hands the renderer, in a form that can be compared.
    fn merman_reading(source: &str) -> String {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source}: {error}"))
            .expect("a diagram");
        format!("{:?}", parsed.model())
    }

    /// Every example `merman`'s own tests draw either opens and survives a
    /// round trip that still draws, or is refused with a reason.
    #[test]
    fn merman_examples_open_and_round_trip_or_are_refused() {
        let fixtures = crate::merman_fixtures("treemap");
        if fixtures.is_empty() {
            eprintln!("merman's treemap fixtures were not found; skipping");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (path, text) in &fixtures {
            let source = text.replace("\r\n", "\n");
            let source = source.trim_end();
            match parse_treemap(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_treemap(&document);
                    let again = parse_treemap(&emitted).unwrap_or_else(|error| {
                        panic!("{path:?} wrote back source that does not open: {error}\n{emitted}")
                    });
                    assert_eq!(again, document, "{path:?} changed through a round trip");
                    assert_eq!(emit_treemap(&again), emitted, "{path:?} is not stable");
                    assert_eq!(
                        merman_reading(&emitted),
                        merman_reading(source),
                        "{path:?} is read differently by merman once written back:\n{emitted}"
                    );
                    if document.structure_error().is_none() {
                        assert_renders(&emitted);
                    }
                }
                Err(error) => refused.push(format!(
                    "{}: {error}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
        eprintln!(
            "treemap fixtures: {opened} opened, {} refused",
            refused.len()
        );
        for reason in &refused {
            eprintln!("  {reason}");
        }
        assert!(
            opened * 4 >= fixtures.len() * 3,
            "too few of merman's examples open: {opened} of {}",
            fixtures.len()
        );
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in ["treemap-beta\n    \"A\": 1", "treemap\n    \"A\": 1"] {
            assert!(is_treemap_source(source), "{source} should be a treemap");
        }
        for source in [
            "treemaps",
            "Treemap-beta",
            "treemap-betas",
            "",
            "%% treemap",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_treemap_source(source),
                "{source} should not be a treemap"
            );
        }
    }
}
