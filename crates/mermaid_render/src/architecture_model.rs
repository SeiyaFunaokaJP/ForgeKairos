//! A round-trippable editing model for Mermaid architecture diagrams
//! (`architecture-beta`).
//!
//! The shape of this module mirrors [`crate::timeline_model`]:
//! [`parse_architecture`] reads the body of a ```` ```mermaid ```` fence into
//! [`ArchitectureDocument`], and [`emit_architecture`] writes it back. See
//! [`crate::mermaid_model`] for the invariants every diagram kind keeps.
//!
//! `merman` reads an architecture diagram one line at a time, and this module
//! reads it with the same rules:
//!
//! - A `%%` outside quotes ends the statement; what follows is a comment. The
//!   model carries a comment written after a group, service, junction or edge
//!   on that statement and writes it back there.
//! - `group` and `service` have to name a parent group declared above them, and
//!   an edge both of its ends. `merman` keeps groups, nodes and edges in three
//!   separate lists, so the interleaving of the three is not part of the
//!   diagram, and the model writes the groups first (every parent above its
//!   children), then the services and junctions, then the edges.
//! - An arrow head is written `<` or `>` on either end; the model writes `<` on
//!   the left and `>` on the right, which read the same.
//! - `merman` 0.8 reads a `[title]` bare only when it is ASCII letters, digits,
//!   `_` and spaces; anything else (Japanese included) has to be quoted, and the
//!   model writes it in double quotes.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! quoting of a title (bare where it can be, otherwise `"…"`), the
//! indentation and the whitespace between the parts of a statement, the order
//! described above, a statement written on the `architecture-beta` line (moved
//! to a line of its own) and comment lines in the middle of the body, which
//! move below it as they do in a timeline.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADER: &str = "architecture-beta";

/// The icons Mermaid draws without an icon pack, with the names the editor
/// offers them under.
pub const KNOWN_ICONS: [(&str, &str); 5] = [
    ("cloud", i18n::msg!("Cloud")),
    ("database", i18n::msg!("Database")),
    ("disk", i18n::msg!("Disk")),
    ("internet", i18n::msg!("Internet")),
    ("server", i18n::msg!("Server")),
];

/// The side of a service, junction or group an edge is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchitectureSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl ArchitectureSide {
    pub const ALL: [ArchitectureSide; 4] = [
        ArchitectureSide::Left,
        ArchitectureSide::Right,
        ArchitectureSide::Top,
        ArchitectureSide::Bottom,
    ];

    pub fn letter(self) -> char {
        match self {
            Self::Left => 'L',
            Self::Right => 'R',
            Self::Top => 'T',
            Self::Bottom => 'B',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Left => i18n::tc!("architecture side", "Left"),
            Self::Right => i18n::tc!("architecture side", "Right"),
            Self::Top => i18n::tc!("architecture side", "Top"),
            Self::Bottom => i18n::tc!("architecture side", "Bottom"),
        }
    }

    fn from_letter(letter: char) -> Option<Self> {
        Self::ALL.into_iter().find(|side| side.letter() == letter)
    }
}

/// What a service shows in its box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchitectureIcon {
    /// `(name)`: one of [`KNOWN_ICONS`] or a `pack:name` icon.
    Named(String),
    /// `"text"`: text drawn in place of an icon, held as written between the
    /// quotes, escapes included, because `merman` keeps it that way.
    Text { text: String, quote: char },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureGroup {
    pub id: String,
    pub icon: Option<String>,
    pub title: Option<String>,
    /// The group this one is drawn inside (`in <id>`).
    pub parent: Option<String>,
    /// A `%%` comment written after the statement, from the `%%` on.
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchitectureNodeKind {
    Service,
    /// A point edges can meet at, drawn without a box. It takes no icon and no
    /// title.
    Junction,
}

impl ArchitectureNodeKind {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Junction => "junction",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Service => i18n::tc!("architecture node", "Service"),
            Self::Junction => i18n::tc!("architecture node", "Junction"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureNode {
    pub kind: ArchitectureNodeKind,
    pub id: String,
    pub icon: Option<ArchitectureIcon>,
    pub title: Option<String>,
    pub parent: Option<String>,
    pub comment: Option<String>,
}

/// One end of an edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureEndpoint {
    /// A service or junction.
    pub id: String,
    pub side: ArchitectureSide,
    /// Whether the edge ends in an arrow head at this end.
    pub arrow: bool,
    /// `{group}`: the edge leaves the group the service is in rather than the
    /// service itself.
    pub group_boundary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureEdge {
    pub from: ArchitectureEndpoint,
    pub to: ArchitectureEndpoint,
    /// `-[title]-`.
    pub title: Option<String>,
    pub comment: Option<String>,
}

/// An editable architecture diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureDocument {
    /// The `title` statement's text. `title` with nothing after it draws no
    /// title, so it is `None`.
    pub title: Option<String>,
    pub groups: Vec<ArchitectureGroup>,
    /// Services and junctions, in the order they are declared.
    pub nodes: Vec<ArchitectureNode>,
    pub edges: Vec<ArchitectureEdge>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl ArchitectureDocument {
    /// A blank architecture diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            groups: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every id the diagram declares, groups first.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.groups
            .iter()
            .map(|group| group.id.as_str())
            .chain(self.nodes.iter().map(|node| node.id.as_str()))
    }

    /// `prefix`, or `prefix` with the first number after it that no group,
    /// service or junction has taken and that `merman` reads as an id:
    /// `service` and `group` themselves are keywords it refuses.
    pub fn unused_id(&self, prefix: &str) -> String {
        let taken = self.ids().collect::<Vec<_>>();
        std::iter::once(prefix.to_string())
            .chain((2..).map(|number| format!("{prefix}{number}")))
            // A prefix no number turns into an id would otherwise search forever.
            .take(10_000)
            .find(|candidate| {
                is_valid_architecture_id(candidate) && !taken.contains(&candidate.as_str())
            })
            .unwrap_or_else(|| prefix.to_string())
    }

    /// Points every `in` and every edge end at the new spelling of the ids in
    /// `renames` (old, new). Each reference is looked up once, so two ids that
    /// trade names keep pointing at the rows they pointed at.
    pub fn retarget(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let rename = |id: &mut String| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == id) {
                *id = new.clone();
            }
        };
        for group in &mut self.groups {
            if let Some(parent) = &mut group.parent {
                rename(parent);
            }
        }
        for node in &mut self.nodes {
            if let Some(parent) = &mut node.parent {
                rename(parent);
            }
        }
        for edge in &mut self.edges {
            rename(&mut edge.from.id);
            rename(&mut edge.to.id);
        }
    }

    /// Removes the group at `index`. What was drawn inside it moves into the
    /// group it was itself inside, and any edge that ended on the group goes
    /// with it, since it would name a group that is no longer declared.
    /// Returns the indices the removed edges had, in ascending order.
    pub fn remove_group(&mut self, index: usize) -> Vec<usize> {
        if index >= self.groups.len() {
            return Vec::new();
        }
        let removed = self.groups.remove(index);
        for group in &mut self.groups {
            if group.parent.as_deref() == Some(removed.id.as_str()) {
                group.parent = removed.parent.clone();
            }
        }
        for node in &mut self.nodes {
            if node.parent.as_deref() == Some(removed.id.as_str()) {
                node.parent = removed.parent.clone();
            }
        }
        self.remove_edges_touching(&removed.id)
    }

    /// Removes the service or junction at `index` and the edges that end on
    /// it. Returns the indices the removed edges had, in ascending order.
    pub fn remove_node(&mut self, index: usize) -> Vec<usize> {
        if index >= self.nodes.len() {
            return Vec::new();
        }
        let removed = self.nodes.remove(index);
        self.remove_edges_touching(&removed.id)
    }

    fn remove_edges_touching(&mut self, id: &str) -> Vec<usize> {
        let removed = self
            .edges
            .iter()
            .enumerate()
            .filter(|(_, edge)| edge.from.id == id || edge.to.id == id)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in removed.iter().rev() {
            self.edges.remove(*index);
        }
        removed
    }

    /// The groups in the order they are written: each below the group it is
    /// in, otherwise in list order. `merman` refuses a parent declared below
    /// its child, and the form lets a parent be picked from further down.
    fn groups_in_writing_order(&self) -> Vec<&ArchitectureGroup> {
        let mut written: Vec<&ArchitectureGroup> = Vec::with_capacity(self.groups.len());
        let mut pending: Vec<&ArchitectureGroup> = self.groups.iter().collect();
        while !pending.is_empty() {
            let ready = pending.iter().position(|group| {
                group
                    .parent
                    .as_deref()
                    .is_none_or(|parent| !pending.iter().any(|other| other.id == parent))
            });
            // A cycle is reported by `structure_error`; writing the rest in
            // list order keeps every group in the text meanwhile.
            written.push(pending.remove(ready.unwrap_or(0)));
        }
        written
    }

    /// Why the diagram cannot be written back as one Mermaid draws the way the
    /// form shows it, in Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_architecture_diagram_title(title)
        {
            return Some(
                i18n::t!("The title cannot be empty or contain line breaks or `%%` (and cannot start or end with whitespace).").to_string(),
            );
        }

        for group in &self.groups {
            if !is_valid_architecture_id(&group.id) {
                return Some(invalid_id_message(&group.id));
            }
            if let Some(icon) = &group.icon
                && !is_valid_architecture_icon_name(icon)
            {
                return Some(invalid_icon_message(&group.id));
            }
            if let Some(title) = &group.title
                && !is_valid_architecture_label(title)
            {
                return Some(invalid_label_message(&group.id));
            }
        }
        for node in &self.nodes {
            if !is_valid_architecture_id(&node.id) {
                return Some(invalid_id_message(&node.id));
            }
            match node.kind {
                ArchitectureNodeKind::Service => {
                    if let Some(icon) = &node.icon
                        && !is_valid_architecture_icon(icon)
                    {
                        return Some(invalid_icon_message(&node.id));
                    }
                    if let Some(title) = &node.title
                        && !is_valid_architecture_label(title)
                    {
                        return Some(invalid_label_message(&node.id));
                    }
                }
                ArchitectureNodeKind::Junction => {
                    if node.icon.is_some() || node.title.is_some() {
                        return Some(i18n::t!(
                            "Junction `{}` cannot have an icon or a title.",
                            node.id
                        ));
                    }
                }
            }
        }

        let mut seen: Vec<&str> = Vec::new();
        for id in self.ids() {
            if seen.contains(&id) {
                return Some(i18n::t!(
                    "The ID `{id}` is used twice. Groups, services and junctions all need different IDs."
                ));
            }
            seen.push(id);
        }

        let is_group = |id: &str| self.groups.iter().any(|group| group.id == id);
        let parents = self
            .groups
            .iter()
            .map(|group| (group.id.as_str(), group.parent.as_deref()))
            .chain(
                self.nodes
                    .iter()
                    .map(|node| (node.id.as_str(), node.parent.as_deref())),
            );
        for (id, parent) in parents {
            let Some(parent) = parent else {
                continue;
            };
            if parent == id {
                return Some(i18n::t!("`{id}` cannot be placed inside itself."));
            }
            if !is_group(parent) {
                return Some(i18n::t!(
                    "`{parent}`, which `{id}` belongs to, is not a group (it does not exist, or it is a service or a junction)."
                ));
            }
        }
        for group in &self.groups {
            let mut current = group.parent.as_deref();
            let mut steps = 0;
            while let Some(parent) = current {
                if parent == group.id || steps > self.groups.len() {
                    return Some(i18n::t!(
                        "The memberships of group `{}` form a loop. Remove one of them.",
                        group.id
                    ));
                }
                steps += 1;
                current = self
                    .groups
                    .iter()
                    .find(|other| other.id == parent)
                    .and_then(|other| other.parent.as_deref());
            }
        }

        for edge in &self.edges {
            for end in [&edge.from, &edge.to] {
                if !seen.contains(&end.id.as_str()) {
                    return Some(i18n::t!(
                        "The edge end `{}` does not exist. Choose the ID of a service or junction.",
                        end.id
                    ));
                }
                // `merman` reads an edge that ends on a group but cannot lay it
                // out, so the diagram would not draw at all.
                if is_group(&end.id) {
                    return Some(i18n::t!(
                        "The edge end `{}` is a group. Connect edges to services or junctions (to leave from the border of a group, use “Group Edge”).",
                        end.id
                    ));
                }
            }
            if let Some(title) = &edge.title
                && !is_valid_architecture_label(title)
            {
                return Some(i18n::t!(
                    "The label `{title}` of the edge `{}` → `{}` cannot be written ({label_rule}).",
                    edge.from.id,
                    edge.to.id,
                    label_rule = label_rule()
                ));
            }
            if edge.from.group_boundary || edge.to.group_boundary {
                let parent_of = |id: &str| {
                    self.nodes
                        .iter()
                        .find(|node| node.id == id)
                        .map(|node| node.parent.as_deref())
                };
                // `merman` refuses a group end on an edge between two nodes of
                // one group, since there is no boundary for it to cross.
                if let (Some(Some(from)), Some(Some(to))) =
                    (parent_of(&edge.from.id), parent_of(&edge.to.id))
                    && from == to
                {
                    return Some(i18n::t!(
                        "The edge `{}` → `{}` stays inside one group, so it cannot use “Group Edge”.",
                        edge.from.id,
                        edge.to.id
                    ));
                }
            }
        }

        for (statement, comment) in self.commented_statements() {
            if !comment_survives(&statement, comment) {
                return Some(i18n::t!(
                    "Mermaid would no longer read the comment after `{statement}` as a comment. Close the quotes (`\"` `'`), or stop using the tag attribute `=\"…\"`."
                ));
            }
        }
        None
    }

    fn commented_statements(&self) -> Vec<(String, &str)> {
        let groups = self
            .groups
            .iter()
            .filter_map(|group| Some((group_statement(group), group.comment.as_deref()?)));
        let nodes = self
            .nodes
            .iter()
            .filter_map(|node| Some((node_statement(node), node.comment.as_deref()?)));
        let edges = self
            .edges
            .iter()
            .filter_map(|edge| Some((edge_statement(edge), edge.comment.as_deref()?)));
        groups.chain(nodes).chain(edges).collect()
    }
}

fn invalid_id_message(id: &str) -> String {
    i18n::t!(
        "The ID `{id}` cannot be used (letters, digits, `_` and `-`, starting and ending with a letter, digit or `_`. IDs that start with a capital L, R, T or B, IDs that start with `title`, and `group`, `service`, `junction`, `in`, `align`, `row`, `column`, `accTitle` and `accDescr` are not allowed)."
    )
}

fn invalid_icon_message(id: &str) -> String {
    i18n::t!(
        "The icon of `{id}` cannot be written (a name uses only letters, digits, `_`, `-` and `:`; to show text, enclose it in quotes)."
    )
}

fn label_rule() -> &'static str {
    i18n::t!(
        "no `%%` or line breaks, no whitespace at either end, and no `\\` before a quote or at the end"
    )
}

fn invalid_label_message(id: &str) -> String {
    i18n::t!(
        "The title of `{id}` cannot be written ({label_rule}).",
        label_rule = label_rule()
    )
}

/// Whether a statement followed by `comment` is still cut at the comment,
/// which a quote left open in the statement would prevent.
fn comment_survives(statement: &str, comment: &str) -> bool {
    // `merman` turns `="` inside an HTML tag into `='` before it looks for
    // the comment, which moves where the quotes open and close.
    if statement.contains('<') && statement.contains("=\"") {
        return false;
    }
    let line = format!("{statement} {comment}");
    comment_start(&line) == Some(statement.len() + 1)
}

/// Whether `id` is read back as the whole id: word characters and `-`, starting
/// and ending with a word character, the way `merman` cuts an id. `accTitle`
/// and `accDescr` at the start of an edge would be read as accessibility
/// statements, so neither can be an id.
///
/// `merman` 0.8 also refuses the ids Mermaid's lexer would split into a
/// keyword and the rest: the statement keywords, anything starting with
/// `title`, and anything starting with a capital `L` `R` `T` or `B` (a side).
pub fn is_valid_architecture_id(id: &str) -> bool {
    let is_word = |character: char| character.is_ascii_alphanumeric() || character == '_';
    id.chars().next().is_some_and(is_word)
        && id.chars().last().is_some_and(is_word)
        && id
            .chars()
            .all(|character| is_word(character) || character == '-')
        && id != "accTitle"
        && id != "accDescr"
        && !matches!(
            id,
            "architecture-beta"
                | "group"
                | "service"
                | "junction"
                | "in"
                | "align"
                | "row"
                | "column"
        )
        && !id.starts_with("title")
        && !id.starts_with(['L', 'R', 'T', 'B'])
}

/// Whether `title` can follow the `title` keyword: one line, trimmed, and no
/// `%%`, which would start a comment.
pub fn is_valid_architecture_diagram_title(title: &str) -> bool {
    !title.is_empty()
        && title.trim() == title
        && !title.contains(['\n', '\r'])
        && !title.contains("%%")
}

/// Whether `label` can be written between `[` and `]` — as a group's or a
/// service's title, or on an edge — and read back as itself. See
/// [`label_text`] for how it is written: `merman` trims it, and in quotes turns
/// `\"` and `\'` into the quote alone, so a backslash in front of a quote or
/// at the end would not come back.
pub fn is_valid_architecture_label(label: &str) -> bool {
    label.trim() == label
        && !label.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
        && !label.contains("%%")
        && !label.contains("\\\"")
        && !label.contains("\\'")
        && !label.ends_with('\\')
}

/// Whether `label` can be written between the brackets without quotes, which
/// `merman` 0.8 allows only for ASCII letters, digits, `_` and spaces.
fn is_plain_architecture_label(label: &str) -> bool {
    !label.trim().is_empty()
        && label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | ' '))
}

/// `label` in the brackets it is written in: bare when `merman` reads it bare,
/// otherwise in double quotes with the quotes inside escaped.
fn label_text(label: &str) -> String {
    if is_plain_architecture_label(label) {
        format!("[{label}]")
    } else {
        format!("[\"{}\"]", label.replace('"', "\\\""))
    }
}

/// Whether `name` can be written between `(` and `)`: `merman` 0.8 reads only
/// ASCII letters, digits, `_`, `-` and `:` there, and at least one of them.
pub fn is_valid_architecture_icon_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':')
        })
}

/// Whether `icon` is written back as itself.
pub fn is_valid_architecture_icon(icon: &ArchitectureIcon) -> bool {
    match icon {
        ArchitectureIcon::Named(name) => is_valid_architecture_icon_name(name),
        ArchitectureIcon::Text { text, quote } => {
            let written = format!("{quote}{text}{quote}");
            // `merman` rewrites `="` inside an HTML tag to `='` before it
            // parses, which would close text quoted with `'` early.
            let attribute_rewritten = *quote == '\'' && text.contains('<') && text.contains("=\"");
            matches!(quote, '"' | '\'')
                && !attribute_rewritten
                && !text.contains(DIRECTIVE_START)
                && !text.contains(['\n', '\r'])
                && take_quoted(&written)
                    .is_some_and(|(inner, rest)| inner == text.as_str() && rest.is_empty())
        }
    }
}

/// How an icon is shown in the form's one icon field: a name as it is, text in
/// its quotes, and the empty `()` as `()` so that it is not taken for "no
/// icon".
pub fn icon_field_text(icon: Option<&ArchitectureIcon>) -> String {
    match icon {
        None => String::new(),
        Some(ArchitectureIcon::Named(name)) if name.is_empty() => "()".to_string(),
        Some(ArchitectureIcon::Named(name)) => name.clone(),
        Some(ArchitectureIcon::Text { text, quote }) => format!("{quote}{text}{quote}"),
    }
}

/// The icon the form's icon field describes; the inverse of
/// [`icon_field_text`]. Text in quotes is icon text and anything else a name, so
/// a quote left open reads as a name the validation then refuses.
pub fn icon_from_field(text: &str) -> Option<ArchitectureIcon> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text == "()" {
        return Some(ArchitectureIcon::Named(String::new()));
    }
    for quote in ['"', '\''] {
        if text.len() >= 2
            && let Some(inner) = text
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))
        {
            return Some(ArchitectureIcon::Text {
                text: inner.to_string(),
                quote,
            });
        }
    }
    Some(ArchitectureIcon::Named(text.to_string()))
}

/// Whether `source` is an architecture diagram: its first statement is `architecture-beta`.
pub fn is_architecture_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_architecture(source: &str) -> Result<ArchitectureDocument> {
    let document = scan_architecture(source)?;
    // `merman` itself lets a junction repeat an id or sit in a group that is
    // not there, which the form could not show.
    if let Some(error) = document.structure_error() {
        bail!(i18n::t!("{error} Edit it as text."));
    }

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!(i18n::t!(
                    "Mermaid could not parse this diagram ({error}). Edit it as text."
                ))
            })?
            .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
        if !matches!(parsed.model(), RenderSemanticModel::Architecture(_)) {
            bail!(i18n::t!("This diagram is not an architecture diagram."));
        }
    }

    Ok(document)
}

fn scan_architecture(source: &str) -> Result<ArchitectureDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = ArchitectureDocument::empty();
    document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default();
    if header.contains(DIRECTIVE_START) {
        bail!(directive_not_alone());
    }
    let (header_statement, header_comment) = split_comment(header);
    let Some(tail) = keyword_tail(header_statement.trim(), HEADER) else {
        bail!(i18n::t!(
            "This is not an architecture diagram (only diagrams that start with `architecture-beta` can be edited in the structured editor)."
        ));
    };
    let mut scanner = Scanner {
        document,
        position: PreservedPosition::Top,
        title_seen: false,
    };
    if let Some(comment) = header_comment {
        scanner.preserve(format!("{INDENT}{comment}"));
    }
    if !tail.is_empty() {
        // `merman` reads what follows the keyword as the first statement.
        match classify(tail) {
            Statement::Accessibility { .. } => bail!(i18n::t!(
                "`{tail}` following the `architecture-beta` line is not supported by the structured editor. Move it to a line of its own, or edit it as text."
            )),
            statement => scanner.read(statement, tail, None)?,
        }
    }

    let mut index = header_index + 1;
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
            let block = join_lines(lines.get(start..index).unwrap_or_default());
            if !is_comment_after_directives(&block, index >= lines.len()) {
                bail!(directive_not_alone());
            }
            scanner.preserve(block);
            continue;
        }
        // `merman` cuts `%%{...}%%` out of the text before it reads any
        // statement, quotes or not, so a statement holding one means
        // something other than what it says.
        if line.contains(DIRECTIVE_START) {
            bail!(directive_not_alone());
        }

        let (statement, comment) = split_comment(line);
        let statement = statement.trim();
        match classify(statement) {
            Statement::Accessibility { block } => {
                // `merman` looks for the closing brace in the statement with
                // its comment removed, then in the raw lines below it.
                let opened = statement
                    .find('{')
                    .and_then(|brace| statement.get(brace..))
                    .unwrap_or_default();
                if block && !opened.contains('}') {
                    while lines.get(index).is_some_and(|line| !line.contains('}')) {
                        index += 1;
                    }
                    if index < lines.len() {
                        index += 1;
                    }
                }
                scanner.preserve(join_lines(lines.get(start..index).unwrap_or_default()));
            }
            kind => scanner.read(kind, statement, comment)?,
        }
    }

    Ok(scanner.document)
}

struct Scanner {
    document: ArchitectureDocument,
    position: PreservedPosition,
    title_seen: bool,
}

impl Scanner {
    fn preserve(&mut self, text: String) {
        self.document.preserved_lines.push(PreservedLine {
            position: self.position,
            text,
        });
    }

    fn read(&mut self, kind: Statement<'_>, statement: &str, comment: Option<&str>) -> Result<()> {
        let comment = comment.map(str::to_string);
        match kind {
            Statement::Title(value) => {
                // Mermaid keeps the last one, so a second statement — even an
                // empty one, which clears the title — would make writing the
                // diagram back drop a line the user can see.
                if self.title_seen {
                    bail!(i18n::t!(
                        "`title` is written twice. Merge them into one, then open it again."
                    ));
                }
                self.title_seen = true;
                let value = value.trim();
                if !value.is_empty() {
                    self.document.title = Some(value.to_string());
                }
                if let Some(comment) = comment {
                    self.preserve(format!("{INDENT}{comment}"));
                }
            }
            Statement::Accessibility { .. } => {
                self.preserve(format!("{INDENT}{statement}"));
            }
            Statement::Group(rest) => {
                let mut group = parse_group(rest, statement)?;
                group.comment = comment;
                self.document.groups.push(group);
                self.position = PreservedPosition::Bottom;
            }
            Statement::Node(kind, rest) => {
                let mut node = parse_node(kind, rest, statement)?;
                node.comment = comment;
                self.document.nodes.push(node);
                self.position = PreservedPosition::Bottom;
            }
            Statement::Edge => {
                let mut edge = parse_edge(statement)?;
                edge.comment = comment;
                self.document.edges.push(edge);
                self.position = PreservedPosition::Bottom;
            }
        }
        Ok(())
    }
}

/// What one statement of an architecture body is, decided in the order
/// `merman`'s parser tries the possibilities.
#[derive(Debug, PartialEq, Eq)]
enum Statement<'a> {
    Title(&'a str),
    Accessibility { block: bool },
    Group(&'a str),
    Node(ArchitectureNodeKind, &'a str),
    Edge,
}

fn classify(statement: &str) -> Statement<'_> {
    if let Some(rest) = keyword_rest(statement, "title") {
        // `merman` drops the one whitespace character after the keyword.
        let value = rest.strip_prefix(char::is_whitespace).unwrap_or_default();
        return Statement::Title(value);
    }
    for keyword in ["accTitle", "accDescr"] {
        if let Some(rest) = statement.strip_prefix(keyword) {
            let rest = rest.trim_start();
            if rest.starts_with(':') {
                return Statement::Accessibility { block: false };
            }
            if keyword == "accDescr" && rest.starts_with('{') {
                return Statement::Accessibility { block: true };
            }
        }
    }
    if let Some(rest) = keyword_rest(statement, "group") {
        return Statement::Group(rest.trim_start());
    }
    for kind in [
        ArchitectureNodeKind::Service,
        ArchitectureNodeKind::Junction,
    ] {
        if let Some(rest) = keyword_rest(statement, kind.keyword()) {
            return Statement::Node(kind, rest.trim_start());
        }
    }
    Statement::Edge
}

/// What follows `keyword` at the start of `line` when the keyword is followed
/// by whitespace or ends the line, which is how `merman` ends one.
fn keyword_rest<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

const DIRECTIVE_START: &str = "%%{";
const DIRECTIVE_END: &str = "}%%";
fn directive_not_alone() -> String {
    i18n::t!(
        "A `%%{{` directive is on the same line as a statement or a comment (Mermaid cuts out only the directive and reads the rest as a statement). Write the directive on a line of its own, or edit it as text."
    )
}

/// Whether a body line (or a multi-line directive) that opens with `%%` is
/// still nothing but comments once `merman` has cut the directives out of it.
/// `merman` cuts from `%%{` to the next `}%%` anywhere in the text, and drops
/// everything after a `%%{` that is never closed, which is only harmless when
/// nothing follows.
fn is_comment_after_directives(block: &str, reaches_the_end: bool) -> bool {
    if !block.contains(DIRECTIVE_START) {
        return true;
    }
    let mut remainder = String::new();
    let mut rest = block;
    while let Some(start) = rest.find(DIRECTIVE_START) {
        remainder.push_str(rest.get(..start).unwrap_or_default());
        let after = rest
            .get(start + DIRECTIVE_START.len()..)
            .unwrap_or_default();
        let Some(end) = after.find(DIRECTIVE_END) else {
            return reaches_the_end && is_only_comments(&remainder);
        };
        rest = after.get(end + DIRECTIVE_END.len()..).unwrap_or_default();
    }
    remainder.push_str(rest);
    is_only_comments(&remainder)
}

fn is_only_comments(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with("%%"))
}

/// Where the `%%` comment in `line` starts, skipping a `%%` inside quotes the
/// way `merman` does.
fn comment_start(line: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut characters = line.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if let Some(open) = quote {
            if character == '\\' {
                characters.next();
            } else if character == open {
                quote = None;
            }
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            continue;
        }
        if character == '%' && characters.peek().is_some_and(|(_, next)| *next == '%') {
            return Some(offset);
        }
    }
    None
}

/// `line` cut at its comment, and the comment from its `%%` on when there is
/// text after the marker.
fn split_comment(line: &str) -> (&str, Option<&str>) {
    let Some(start) = comment_start(line) else {
        return (line, None);
    };
    let statement = line.get(..start).unwrap_or_default();
    let comment = line.get(start..).unwrap_or_default().trim_end();
    (statement, (comment.len() > "%%".len()).then_some(comment))
}

/// The id at the start of `input`, cut the way `merman` cuts one: word
/// characters and `-`, ending at the last word character.
fn take_id(input: &str) -> Option<(&str, &str)> {
    let mut end = None;
    for (offset, character) in input.char_indices() {
        let is_word = character.is_ascii_alphanumeric() || character == '_';
        if end.is_none() {
            if !is_word {
                return None;
            }
            end = Some(offset + character.len_utf8());
            continue;
        }
        if !is_word && character != '-' {
            break;
        }
        if is_word {
            end = Some(offset + character.len_utf8());
        }
    }
    let end = end?;
    Some((input.get(..end)?, input.get(end..)?))
}

fn take_bracketed(input: &str, open: char, close: char) -> Option<(&str, &str)> {
    let rest = input.strip_prefix(open)?;
    let end = rest.find(close)?;
    Some((rest.get(..end)?, rest.get(end + close.len_utf8()..)?))
}

/// A `[title]` at the start of `input` as `merman` 0.8 reads it: bare up to the
/// `]`, or in quotes straight after the `[` with `\"` and `\'` unescaped, and
/// trimmed either way. What `merman` refuses inside is left to it.
fn take_label(input: &str) -> Option<(String, &str)> {
    let rest = input.strip_prefix('[')?;
    if let Some((inner, tail)) = take_quoted(rest) {
        let tail = tail.strip_prefix(']')?;
        let text = inner.replace("\\\"", "\"").replace("\\'", "'");
        return Some((text.trim().to_string(), tail));
    }
    let (text, tail) = take_bracketed(input, '[', ']')?;
    Some((text.trim().to_string(), tail))
}

fn take_quoted(input: &str) -> Option<(&str, &str)> {
    let quote = input
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))?;
    let body = input.get(quote.len_utf8()..)?;
    let mut escaped = false;
    for (offset, character) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            return Some((body.get(..offset)?, body.get(offset + quote.len_utf8()..)?));
        }
    }
    None
}

/// Reads `in <group>` and checks that nothing follows it.
fn take_parent(rest: &str, statement: &str) -> Result<Option<String>> {
    let rest = rest.trim_start();
    let (parent, rest) = match keyword_rest(rest, "in") {
        Some(after) => {
            let Some((parent, tail)) = take_id(after.trim_start()) else {
                bail!(i18n::t!(
                    "Could not read the group ID after `in` ({statement})."
                ));
            };
            (Some(parent.to_string()), tail)
        }
        None => (None, rest),
    };
    if !rest.trim().is_empty() {
        bail!(i18n::t!(
            "Could not read `{}` in `{statement}`.",
            rest.trim()
        ));
    }
    Ok(parent)
}

fn parse_group(rest: &str, statement: &str) -> Result<ArchitectureGroup> {
    let Some((id, mut rest)) = take_id(rest) else {
        bail!(i18n::t!("Could not read the group ID ({statement})."));
    };
    rest = rest.trim_start();
    let mut icon = None;
    if let Some((name, tail)) = take_bracketed(rest, '(', ')') {
        icon = Some(name.trim().to_string());
        rest = tail.trim_start();
    }
    let mut title = None;
    if let Some((text, tail)) = take_label(rest) {
        title = Some(text);
        rest = tail;
    }
    Ok(ArchitectureGroup {
        id: id.to_string(),
        icon,
        title,
        parent: take_parent(rest, statement)?,
        comment: None,
    })
}

fn parse_node(kind: ArchitectureNodeKind, rest: &str, statement: &str) -> Result<ArchitectureNode> {
    let Some((id, mut rest)) = take_id(rest) else {
        bail!(match kind {
            ArchitectureNodeKind::Service => {
                i18n::t!("Could not read the ID of the service ({statement}).")
            }
            ArchitectureNodeKind::Junction => {
                i18n::t!("Could not read the ID of the junction ({statement}).")
            }
        });
    };
    rest = rest.trim_start();
    let mut icon = None;
    let mut title = None;
    if kind == ArchitectureNodeKind::Service {
        if let Some((name, tail)) = take_bracketed(rest, '(', ')') {
            icon = Some(ArchitectureIcon::Named(name.trim().to_string()));
            rest = tail.trim_start();
        } else if let Some((text, tail)) = take_quoted(rest) {
            icon = Some(ArchitectureIcon::Text {
                text: text.to_string(),
                quote: rest.chars().next().unwrap_or('"'),
            });
            rest = tail.trim_start();
        }
        if let Some((text, tail)) = take_label(rest) {
            title = Some(text);
            rest = tail;
        }
    }
    Ok(ArchitectureNode {
        kind,
        id: id.to_string(),
        icon,
        title,
        parent: take_parent(rest, statement)?,
        comment: None,
    })
}

const GROUP_MODIFIER: &str = "{group}";

/// An edge end's id and whether `{group}` follows it, and what is left.
fn take_endpoint<'a>(input: &'a str, statement: &str) -> Result<(String, bool, &'a str)> {
    let Some((id, rest)) = take_id(input) else {
        bail!(i18n::t!(
            "Could not read the ID of an edge end ({statement})."
        ));
    };
    Ok(match rest.strip_prefix(GROUP_MODIFIER) {
        Some(rest) => (id.to_string(), true, rest),
        None => (id.to_string(), false, rest),
    })
}

fn take_side<'a>(input: &'a str, statement: &str) -> Result<(ArchitectureSide, &'a str)> {
    let side = input
        .chars()
        .next()
        .and_then(ArchitectureSide::from_letter)
        .ok_or_else(|| {
            anyhow!(i18n::t!(
                "The side of an edge is one of L R T B ({statement})."
            ))
        })?;
    Ok((side, input.get(1..).unwrap_or_default()))
}

fn parse_edge(statement: &str) -> Result<ArchitectureEdge> {
    let unreadable = || {
        anyhow!(i18n::t!(
            "`{statement}` cannot be read as an edge (write it as `a:R -- L:b`)."
        ))
    };

    let (from_id, from_group, rest) = take_endpoint(statement, statement)?;
    let rest = rest
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(unreadable)?
        .trim_start();
    let (from_side, rest) = take_side(rest, statement)?;
    let mut rest = rest.trim_start();
    let from_arrow = rest.starts_with(['<', '>']);
    if from_arrow {
        rest = rest.get(1..).unwrap_or_default().trim_start();
    }

    let mut title = None;
    if let Some(after) = rest.strip_prefix("--") {
        rest = after;
    } else if let Some(after) = rest.strip_prefix('-') {
        let (text, tail) = take_label(after.trim_start()).ok_or_else(unreadable)?;
        title = Some(text);
        rest = tail.trim_start().strip_prefix('-').ok_or_else(unreadable)?;
    } else {
        return Err(unreadable());
    }

    rest = rest.trim_start();
    let to_arrow = rest.starts_with(['<', '>']);
    if to_arrow {
        rest = rest.get(1..).unwrap_or_default().trim_start();
    }
    let (to_side, rest) = take_side(rest, statement)?;
    let rest = rest
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(unreadable)?
        .trim_start();
    let (to_id, to_group, rest) = take_endpoint(rest, statement)?;
    if !rest.trim().is_empty() {
        return Err(unreadable());
    }

    Ok(ArchitectureEdge {
        from: ArchitectureEndpoint {
            id: from_id,
            side: from_side,
            arrow: from_arrow,
            group_boundary: from_group,
        },
        to: ArchitectureEndpoint {
            id: to_id,
            side: to_side,
            arrow: to_arrow,
            group_boundary: to_group,
        },
        title,
        comment: None,
    })
}

fn group_statement(group: &ArchitectureGroup) -> String {
    let mut out = format!("group {}", group.id);
    if let Some(icon) = &group.icon {
        out.push_str(&format!("({icon})"));
    }
    if let Some(title) = &group.title {
        out.push_str(&label_text(title));
    }
    if let Some(parent) = &group.parent {
        out.push_str(&format!(" in {parent}"));
    }
    out
}

fn node_statement(node: &ArchitectureNode) -> String {
    let mut out = format!("{} {}", node.kind.keyword(), node.id);
    match &node.icon {
        Some(ArchitectureIcon::Named(name)) => out.push_str(&format!("({name})")),
        Some(ArchitectureIcon::Text { text, quote }) => {
            out.push_str(&format!(" {quote}{text}{quote}"));
            if node.title.is_some() {
                out.push(' ');
            }
        }
        None => {}
    }
    if let Some(title) = &node.title {
        out.push_str(&label_text(title));
    }
    if let Some(parent) = &node.parent {
        out.push_str(&format!(" in {parent}"));
    }
    out
}

fn endpoint_text(end: &ArchitectureEndpoint) -> String {
    if end.group_boundary {
        format!("{}{GROUP_MODIFIER}", end.id)
    } else {
        end.id.clone()
    }
}

fn edge_statement(edge: &ArchitectureEdge) -> String {
    let link = match &edge.title {
        Some(title) => format!("-{}-", label_text(title)),
        None => "--".to_string(),
    };
    format!(
        "{}:{} {}{link}{} {}:{}",
        endpoint_text(&edge.from),
        edge.from.side.letter(),
        if edge.from.arrow { "<" } else { "" },
        if edge.to.arrow { ">" } else { "" },
        edge.to.side.letter(),
        endpoint_text(&edge.to),
    )
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_architecture(document: &ArchitectureDocument) -> String {
    let mut out = String::new();
    let push_statement = |out: &mut String, statement: &str, comment: Option<&str>| {
        out.push_str(INDENT);
        out.push_str(statement);
        if let Some(comment) = comment {
            out.push(' ');
            out.push_str(comment);
        }
        out.push('\n');
    };

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(HEADER);
    out.push('\n');
    if let Some(title) = &document.title {
        push_statement(&mut out, &format!("title {title}"), None);
    }
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    for group in document.groups_in_writing_order() {
        push_statement(&mut out, &group_statement(group), group.comment.as_deref());
    }
    for node in &document.nodes {
        push_statement(&mut out, &node_statement(node), node.comment.as_deref());
    }
    for edge in &document.edges {
        push_statement(&mut out, &edge_statement(edge), edge.comment.as_deref());
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
    document: &ArchitectureDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};
    use serde_json::Value;

    const TEMPLATE: &str = "architecture-beta\n    %% group はまとまり、service は部品。:L :R :T :B で線をつなぐ辺を決める\n    group app(cloud)[\"アプリ\"]\n    service web(server)[\"Web サーバー\"] in app\n    service db(database)[\"データベース\"] in app\n    service disk(disk)[\"ストレージ\"] in app\n    web:R -- L:db\n    web:B -- T:disk";

    fn parse(source: &str) -> ArchitectureDocument {
        parse_architecture(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_architecture(&first);
        let second = parse(&first_text);
        let second_text = emit_architecture(&second);
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

    /// What `merman` itself makes of `source`: the parts of its model that the
    /// source decides. The title is trimmed because `merman` keeps the spaces
    /// after the first one.
    fn merman_model(source: &str) -> Option<Value> {
        let parsed = Engine::new()
            .parse_diagram_sync(source, ParseOptions::strict())
            .ok()??;
        let model = parsed.model;
        let title = model
            .get("title")
            .and_then(Value::as_str)
            .map(|title| title.trim().to_string());
        Some(serde_json::json!({
            "title": title,
            "accTitle": model.get("accTitle"),
            "accDescr": model.get("accDescr"),
            "groups": model.get("groups"),
            "nodes": model.get("nodes"),
            "edges": model.get("edges"),
        }))
    }

    /// The strongest check there is: `merman` reads the written back source as
    /// the same diagram it read the original as.
    fn assert_same_diagram(original: &str, emitted: &str) {
        let before = merman_model(original).expect("merman should read the original");
        let after = merman_model(emitted).expect("merman should read the written back source");
        assert_eq!(
            before, after,
            "merman reads a different diagram after the round trip:\n{original}\n---\n{emitted}"
        );
    }

    fn service(
        id: &str,
        icon: Option<&str>,
        title: Option<&str>,
        parent: Option<&str>,
    ) -> ArchitectureNode {
        ArchitectureNode {
            kind: ArchitectureNodeKind::Service,
            id: id.to_string(),
            icon: icon.map(|icon| ArchitectureIcon::Named(icon.to_string())),
            title: title.map(str::to_string),
            parent: parent.map(str::to_string),
            comment: None,
        }
    }

    fn group(id: &str, parent: Option<&str>) -> ArchitectureGroup {
        ArchitectureGroup {
            id: id.to_string(),
            icon: None,
            title: None,
            parent: parent.map(str::to_string),
            comment: None,
        }
    }

    fn end(id: &str, side: ArchitectureSide) -> ArchitectureEndpoint {
        ArchitectureEndpoint {
            id: id.to_string(),
            side,
            arrow: false,
            group_boundary: false,
        }
    }

    fn edge(from: &str, to: &str) -> ArchitectureEdge {
        ArchitectureEdge {
            from: end(from, ArchitectureSide::Right),
            to: end(to, ArchitectureSide::Left),
            title: None,
            comment: None,
        }
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        let source = "architecture-beta\n    service db(database)[DB]";
        assert!(
            is_architecture_source(source),
            "{source} should be an architecture diagram"
        );
        for source in [
            "architecture\n  service db(database)[DB]",
            "architecture-betas",
            "Architecture-beta",
            "",
            "%% architecture",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_architecture_source(source),
                "{source} should not be an architecture diagram"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(
            document.groups,
            vec![ArchitectureGroup {
                icon: Some("cloud".to_string()),
                title: Some("アプリ".to_string()),
                ..group("app", None)
            }]
        );
        assert_eq!(
            document.nodes,
            vec![
                service("web", Some("server"), Some("Web サーバー"), Some("app")),
                service("db", Some("database"), Some("データベース"), Some("app")),
                service("disk", Some("disk"), Some("ストレージ"), Some("app")),
            ]
        );
        assert_eq!(
            document.edges,
            vec![
                edge("web", "db"),
                ArchitectureEdge {
                    from: end("web", ArchitectureSide::Bottom),
                    to: end("disk", ArchitectureSide::Top),
                    title: None,
                    comment: None,
                },
            ]
        );
        assert_eq!(emit_architecture(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_statement_form_survives() {
        let source = "architecture-beta\n    title システム構成\n    group cloud_env(logos:aws)[\"クラウド\"]\n    group private-net(cloud)[\"内部 [VPC] \\\"本番\\\"\"] in cloud_env %% 内側\n    service api(server)[API] in private-net\n    service fn \"<b>λ</b>\" [\"関数\"] in cloud_env\n    service quoted 'a \\' b'\n    service bare\n    service plain(disk)[ Plain label_1 ]\n    junction hub in cloud_env\n    api{group}:R <-[\"呼び出し\"]-> L:fn %% 双方向\n    fn:B -- T:hub\n    hub:L >-- R:bare\n    bare:T --> B:quoted\n    quoted:R -['it\\'s']- L:plain";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("システム構成"));
        assert_eq!(
            document.groups.get(1),
            Some(&ArchitectureGroup {
                icon: Some("cloud".to_string()),
                title: Some("内部 [VPC] \"本番\"".to_string()),
                comment: Some("%% 内側".to_string()),
                ..group("private-net", Some("cloud_env"))
            })
        );
        let fn_node = document.nodes.get(1).expect("fn");
        assert_eq!(
            fn_node.icon,
            Some(ArchitectureIcon::Text {
                text: "<b>λ</b>".to_string(),
                quote: '"'
            })
        );
        assert_eq!(
            document.nodes.get(2).and_then(|node| node.icon.clone()),
            Some(ArchitectureIcon::Text {
                text: "a \\' b".to_string(),
                quote: '\''
            })
        );
        assert_eq!(
            document
                .nodes
                .get(4)
                .map(|node| (node.icon.clone(), node.title.clone())),
            Some((
                Some(ArchitectureIcon::Named("disk".to_string())),
                Some("Plain label_1".to_string())
            ))
        );
        assert_eq!(
            document
                .nodes
                .get(5)
                .map(|node| (node.kind, node.parent.clone())),
            Some((
                ArchitectureNodeKind::Junction,
                Some("cloud_env".to_string())
            ))
        );
        let first_edge = document.edges.first().expect("an edge");
        assert!(first_edge.from.group_boundary && first_edge.from.arrow && first_edge.to.arrow);
        assert_eq!(first_edge.title.as_deref(), Some("呼び出し"));
        assert_eq!(first_edge.comment.as_deref(), Some("%% 双方向"));
        assert!(
            document
                .edges
                .get(2)
                .is_some_and(|edge| edge.from.arrow && !edge.to.arrow)
        );

        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "architecture-beta\n    title システム構成\n    group cloud_env(logos:aws)[\"クラウド\"]\n    group private-net(cloud)[\"内部 [VPC] \\\"本番\\\"\"] in cloud_env %% 内側\n    service api(server)[API] in private-net\n    service fn \"<b>λ</b>\" [\"関数\"] in cloud_env\n    service quoted 'a \\' b'\n    service bare\n    service plain(disk)[Plain label_1]\n    junction hub in cloud_env\n    api{group}:R <-[\"呼び出し\"]-> L:fn %% 双方向\n    fn:B -- T:hub\n    hub:L <-- R:bare\n    bare:T --> B:quoted\n    quoted:R -[\"it's\"]- L:plain"
        );
        assert_same_diagram(source, &emitted);
        assert_renders(&emitted);
    }

    #[test]
    fn comments_accessibility_and_the_preamble_keep_their_place() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\narchitecture-beta %% 見出しのメモ\n    %% 先頭のメモ\n    accTitle: 概要\n    title 構成 %% 題のメモ\n    service a(server)[A]\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ\n    %%\n    service b(disk)[B]\n    a:R -- L:b";
        let document = parse(source);
        let positions = document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.trim().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                (
                    PreservedPosition::Preamble,
                    "---\ntitle: 図\n---".to_string()
                ),
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 見出しのメモ".to_string()),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Top, "accTitle: 概要".to_string()),
                (PreservedPosition::Top, "%% 題のメモ".to_string()),
                (
                    PreservedPosition::Bottom,
                    "accDescr {\n      説明の 1 行目\n      2 行目\n    }".to_string()
                ),
                (PreservedPosition::Bottom, "%% 途中のメモ".to_string()),
                (PreservedPosition::Bottom, "%%".to_string()),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\narchitecture-beta\n    title 構成\n    %% 見出しのメモ\n    %% 先頭のメモ\n    accTitle: 概要\n    %% 題のメモ\n    service a(server)[A]\n    service b(disk)[B]\n    a:R -- L:b\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ\n    %%"
        );
        assert_same_diagram(source, &emitted);
        assert_renders(&emitted);
    }

    #[test]
    fn a_statement_on_the_keyword_line_moves_to_a_line_of_its_own() {
        let source = "architecture-beta title Simple Architecture Diagram\n    service a";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "architecture-beta\n    title Simple Architecture Diagram\n    service a"
        );
        assert_same_diagram(source, &emitted);
    }

    #[test]
    fn interleaved_statements_are_written_in_merman_order() {
        // `merman` 0.7 refused an edge above one of its ends; 0.8 connects the
        // two once every statement is read, so the edge moves below its ends.
        let source = "architecture-beta\n    group a\n    service x in a\n    group b in a\n    x:R -- L:y\n    service y in b";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "architecture-beta\n    group a\n    group b in a\n    service x in a\n    service y in b\n    x:R -- L:y"
        );
        assert_same_diagram(source, &emitted);
        let source = "architecture-beta\n    group a\n    service x in a\n    group b in a\n    service y in b\n    x:R -- L:y\n    junction j\n    y:B -- T:j";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "architecture-beta\n    group a\n    group b in a\n    service x in a\n    service y in b\n    junction j\n    x:R -- L:y\n    y:B -- T:j"
        );
        assert_same_diagram(source, &emitted);
    }

    #[test]
    fn a_parent_picked_from_further_down_is_written_above_its_child() {
        let mut document = ArchitectureDocument::empty();
        document.groups = vec![group("inner", Some("outer")), group("outer", None)];
        document.nodes = vec![service("s", None, None, Some("inner"))];
        assert_eq!(document.structure_error(), None);
        let emitted = emit_architecture(&document);
        assert_eq!(
            emitted,
            "architecture-beta\n    group outer\n    group inner in outer\n    service s in inner"
        );
        assert_renders(&emitted);
        assert_eq!(emit_architecture(&parse(&emitted)), emitted);
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for (source, expected) in [
            ("architecture-beta\n    title a\n    title b", "twice"),
            ("architecture-beta\n    title a\n    title", "twice"),
            (
                "architecture-beta accTitle: x",
                "following the `architecture-beta` line",
            ),
            ("architecture-beta\n    service a\n    junction a", "twice"),
            (
                "architecture-beta\n    junction j in nowhere",
                "is not a group",
            ),
            (
                "architecture-beta\n    service a\n    junction j in a",
                "is not a group",
            ),
            ("architecture-beta\n    service accTitle", "accTitle"),
            ("architecture-beta\n    service a(\"x\")", "icon"),
            (
                "architecture-beta\n    service a\n    a:X -- L:a",
                "L R T B",
            ),
            (
                "architecture-beta\n    service a\n    a:R == L:a",
                "as an edge",
            ),
            ("architecture-beta\n    service a extra", "Could not read"),
        ] {
            let error = parse_architecture(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }
        // What `merman` itself refuses is refused with its reason: since 0.8
        // that includes a bare title that is not ASCII, an empty title or
        // icon, and an id that starts like a keyword or a side.
        for source in [
            "architecture-beta\n    group g[クラウド]",
            "architecture-beta\n    service a[]",
            "architecture-beta\n    service a()",
            "architecture-beta\n    service a(my icon)",
            "architecture-beta\n    service Backend",
            "architecture-beta\n    service service",
            "architecture-beta\n    service title2",
            "architecture-beta\n    service a\n    service b\n    a:R -- L::b",
            "architecture-beta\n    service a\n    service a",
            "architecture-beta\n    service a\n    a:R -- L:b",
            "architecture-beta\n    group g\n    service a in g\n    service b in g\n    a{group}:R -- L:b",
        ] {
            assert!(parse_architecture(source).is_err(), "{source}");
        }
        // `merman` 0.7 dropped a service above the group it is in; 0.8 reads
        // it, and the model writes the group first.
        let source = "architecture-beta\n    service a in g\n    group g";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "architecture-beta\n    group g\n    service a in g"
        );
        assert_same_diagram(source, &emitted);
    }

    #[test]
    fn structural_mistakes_are_reported() {
        let base = || {
            let mut document = ArchitectureDocument::empty();
            document.groups = vec![group("g", None), group("h", Some("g"))];
            document.nodes = vec![
                service("a", Some("server"), Some("A"), Some("g")),
                service("b", None, None, Some("g")),
            ];
            document.edges = vec![edge("a", "b")];
            document
        };
        assert_eq!(base().structure_error(), None);

        let cases: Vec<(Box<dyn Fn(&mut ArchitectureDocument)>, &str)> = vec![
            (
                Box::new(|document| document.groups[0].parent = Some("h".into())),
                "loop",
            ),
            (
                Box::new(|document| document.groups[0].parent = Some("g".into())),
                "inside itself",
            ),
            (
                Box::new(|document| document.nodes[0].parent = Some("b".into())),
                "is not a group",
            ),
            (
                Box::new(|document| document.nodes[1].id = "g".into()),
                "twice",
            ),
            (
                Box::new(|document| document.nodes[1].id = "b-".into()),
                "cannot be used",
            ),
            (
                Box::new(|document| document.edges[0].to.id = "zzz".into()),
                "does not exist",
            ),
            (
                Box::new(|document| document.edges[0].title = Some("a%%b".into())),
                "The label",
            ),
            (
                Box::new(|document| document.edges[0].from.group_boundary = true),
                "“Group Edge”",
            ),
            (
                Box::new(|document| document.nodes[0].title = Some(" A".into())),
                "The title",
            ),
            (
                Box::new(|document| {
                    document.nodes[0].icon = Some(ArchitectureIcon::Named("a)b".into()))
                }),
                "icon",
            ),
            (
                Box::new(|document| document.groups[0].icon = Some("x%%y".into())),
                "icon",
            ),
            (
                Box::new(|document| document.title = Some("a %% b".into())),
                "The title",
            ),
            (
                Box::new(|document| {
                    document.nodes[1].kind = ArchitectureNodeKind::Junction;
                    document.nodes[1].title = Some("J".into());
                }),
                "Junction",
            ),
            (
                Box::new(|document| document.nodes[0].id = "Backend".into()),
                "cannot be used",
            ),
            (
                Box::new(|document| {
                    document.groups[0].icon = Some("日本".into());
                }),
                "icon",
            ),
        ];
        for (index, (edit, expected)) in cases.iter().enumerate() {
            let mut document = base();
            edit(&mut document);
            let error = document
                .structure_error()
                .unwrap_or_else(|| panic!("case {index} should be reported"));
            assert!(error.contains(expected), "case {index}: {error}");
        }

        // A title with an unbalanced quote used to swallow the comment after it
        // when written bare; quoted, the comment stays a comment.
        let mut document = base();
        document.nodes[0].title = Some("it's".into());
        document.nodes[0].comment = Some("%% メモ".into());
        assert_eq!(document.structure_error(), None);
        let emitted = emit_architecture(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");

        // An edge between two groups' members may leave through the boundary.
        let mut document = base();
        document.nodes[1].parent = Some("h".into());
        document.edges[0].from.group_boundary = true;
        document.edges[0].to.group_boundary = true;
        assert_eq!(document.structure_error(), None);
        let emitted = emit_architecture(&document);
        assert_renders(&emitted);
        assert_eq!(parse(&emitted), document);
    }

    #[test]
    fn renames_and_removals_follow_the_references() {
        let mut document = parse(
            "architecture-beta\n    group outer\n    group app in outer\n    service web in app\n    service db in app\n    junction j in outer\n    web:R -- L:db\n    db:B -- T:j",
        );
        document.groups[1].id = "application".into();
        document.nodes[0].id = "db".into();
        document.nodes[1].id = "web".into();
        document.retarget(&[
            ("app".into(), "application".into()),
            ("web".into(), "db".into()),
            ("db".into(), "web".into()),
        ]);
        assert_eq!(document.structure_error(), None);
        assert_eq!(
            emit_architecture(&document),
            "architecture-beta\n    group outer\n    group application in outer\n    service db in application\n    service web in application\n    junction j in outer\n    db:R -- L:web\n    web:B -- T:j"
        );

        let removed = document.remove_group(1);
        assert_eq!(removed, Vec::<usize>::new());
        assert_eq!(document.structure_error(), None);
        assert_eq!(
            emit_architecture(&document),
            "architecture-beta\n    group outer\n    service db in outer\n    service web in outer\n    junction j in outer\n    db:R -- L:web\n    web:B -- T:j"
        );
        let removed = document.remove_node(1);
        assert_eq!(removed, vec![0, 1]);
        assert!(document.edges.is_empty());
        assert_eq!(document.structure_error(), None);
        assert_renders(&emit_architecture(&document));
    }

    /// `merman` reads an edge that ends on a group but its layout cannot place
    /// it, so the diagram does not draw; the form must not write one.
    #[test]
    fn an_edge_cannot_end_on_a_group() {
        let source = "architecture-beta\n    group g\n    service a\n    a:R -- L:g";
        assert!(render_to_svg(source, &MermaidTheme::default()).is_err());
        let error = parse_architecture(source).expect_err("an edge to a group should be refused");
        assert!(format!("{error}").contains("is a group"), "{error}");

        let mut document = ArchitectureDocument::empty();
        document.groups = vec![group("g", None), group("h", None)];
        document.nodes = vec![service("a", None, None, Some("g"))];
        document.edges = vec![edge("g", "h")];
        let error = document.structure_error().expect("a group-to-group edge");
        assert!(error.contains("`g` is a group"), "{error}");
        document.edges = vec![edge("a", "h")];
        let error = document.structure_error().expect("a service-to-group edge");
        assert!(error.contains("`h` is a group"), "{error}");
        assert!(render_to_svg(&emit_architecture(&document), &MermaidTheme::default()).is_err());
    }

    /// `merman` cuts every `%%{...}%%` out of the text before reading
    /// statements, so a directive that shares its line with a statement hides
    /// that statement from the form, and an unclosed one drops what follows.
    #[test]
    fn a_directive_sharing_a_line_is_refused() {
        for source in [
            "architecture-beta\n    service a\n    %%{init: {}}%% service b",
            "architecture-beta\n    group g\n    service a %%{init: {}}%% in g",
            "architecture-beta %%{init: {}}%% service a",
            "architecture-beta\n    service a\n    %% note %%{\n    service b",
            "architecture-beta\n    service a \"x %%{y}%% z\"",
            "architecture-beta\n    %%{init: {\n    \"theme\": \"dark\"}}%% service a",
        ] {
            let error = parse_architecture(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(
                format!("{error}").contains("directive"),
                "{source}: {error}"
            );
        }
        for source in [
            "architecture-beta\n    %%{init: {\"theme\": \"dark\"}}%%\n    service a",
            "architecture-beta\n    service a\n    %%{init: {\n    \"theme\": \"dark\"}}%%\n    service b",
            "architecture-beta\n    service a\n    %% note %%{wrap}%% still a note",
        ] {
            let emitted = assert_stable(source);
            assert_same_diagram(source, &emitted);
        }
        assert!(!is_valid_architecture_icon(&ArchitectureIcon::Text {
            text: "a %%{b".into(),
            quote: '"',
        }));
    }

    /// `merman` rewrites `="` to `='` inside an HTML tag before parsing, which
    /// closes text quoted with `'` early and moves where a comment starts.
    #[test]
    fn html_attributes_that_merman_rewrites_are_refused() {
        let icon = ArchitectureIcon::Text {
            text: "<b c=\"x\">".into(),
            quote: '\'',
        };
        assert!(!is_valid_architecture_icon(&icon));
        let mut document = ArchitectureDocument::empty();
        document.nodes = vec![ArchitectureNode {
            icon: Some(icon),
            ..service("a", None, None, None)
        }];
        assert!(document.structure_error().is_some());
        assert!(parse_architecture(&emit_architecture(&document)).is_err());
        // The same text in double quotes, escaped, is not rewritten.
        let escaped = ArchitectureIcon::Text {
            text: "<b c=\\\"x\\\">".into(),
            quote: '"',
        };
        assert!(is_valid_architecture_icon(&escaped));

        // A title was written bare under `merman` 0.7, where `="` in it moved
        // where the comment starts. Since 0.8 it is written in double quotes
        // with every `"` escaped, so there is no `="` left to rewrite.
        let mut document = ArchitectureDocument::empty();
        document.nodes = vec![ArchitectureNode {
            comment: Some("%% memo".into()),
            ..service("a", None, Some("<i t=\"it's\">"), None)
        }];
        assert_eq!(document.structure_error(), None);
        let emitted = emit_architecture(&document);
        assert_eq!(
            emitted,
            "architecture-beta\n    service a[\"<i t=\\\"it's\\\">\"] %% memo"
        );
        assert_eq!(parse(&emitted), document);
    }

    #[test]
    fn values_are_validated() {
        for id in [
            "a",
            "a-b",
            "a_",
            "_",
            "9",
            "web-server_1",
            "input",
            "groups",
            "backend",
            "l",
            "subtitle",
        ] {
            assert!(is_valid_architecture_id(id), "{id}");
        }
        for id in [
            "", "a-", "-a", "a b", "日本", "a.b", "accTitle", "accDescr", "in", "group", "service",
            "junction", "align", "row", "column", "title", "title2", "Backend", "L", "Rb", "T_1",
            "B-2",
        ] {
            assert!(!is_valid_architecture_id(id), "{id}");
        }
        for label in [
            "",
            "API [v2",
            "日本語",
            "it's",
            "100%",
            "a]b",
            "\"quoted\"",
            "back\\slash",
        ] {
            assert!(is_valid_architecture_label(label), "{label}");
        }
        for label in [" a", "a ", "a%%b", "改行\nあり", "a\\\"b", "a\\'b", "a\\"] {
            assert!(!is_valid_architecture_label(label), "{label}");
        }
        for name in ["cloud", "logos:aws-lambda", "a_b-c:d"] {
            assert!(is_valid_architecture_icon_name(name), "{name}");
        }
        for name in ["", "a(b", "a)", "\"x", "'x", " a", "a%%", "日本", "a b"] {
            assert!(!is_valid_architecture_icon_name(name), "{name}");
        }
        assert!(is_valid_architecture_icon(&ArchitectureIcon::Text {
            text: "a \\\" b".into(),
            quote: '"'
        }));
        assert!(!is_valid_architecture_icon(&ArchitectureIcon::Text {
            text: "a \" b".into(),
            quote: '"'
        }));
        assert!(!is_valid_architecture_icon(&ArchitectureIcon::Text {
            text: "a\\".into(),
            quote: '"'
        }));

        // Everything the predicates accept has to come back as itself.
        let mut document = ArchitectureDocument::empty();
        document.title = Some("構成 100%".into());
        document.groups = vec![ArchitectureGroup {
            icon: Some("logos:a_b-c".into()),
            title: Some("API [v2".into()),
            ..group("input", None)
        }];
        document.nodes = vec![
            ArchitectureNode {
                icon: Some(ArchitectureIcon::Text {
                    text: "it's %% \\\"".into(),
                    quote: '"',
                }),
                title: Some("it's \"a]b\" back\\slash".into()),
                ..service("groups", None, None, Some("input"))
            },
            service("web-server_1", Some("x_1"), Some(""), None),
            service("plain", None, Some("Plain 1_x"), None),
        ];
        document.edges = vec![ArchitectureEdge {
            title: Some("100% \"a]b\"".into()),
            comment: Some("%% メモ".into()),
            ..edge("groups", "web-server_1")
        }];
        assert_eq!(document.structure_error(), None);
        let emitted = emit_architecture(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_renders(&emitted);
    }

    #[test]
    fn the_icon_field_reads_back_what_it_shows() {
        for icon in [
            None,
            Some(ArchitectureIcon::Named("cloud".into())),
            Some(ArchitectureIcon::Named(String::new())),
            Some(ArchitectureIcon::Named("logos:aws".into())),
            Some(ArchitectureIcon::Text {
                text: "λ".into(),
                quote: '"',
            }),
            Some(ArchitectureIcon::Text {
                text: String::new(),
                quote: '\'',
            }),
        ] {
            assert_eq!(icon_from_field(&icon_field_text(icon.as_ref())), icon);
        }
        assert_eq!(
            icon_from_field("\"open"),
            Some(ArchitectureIcon::Named("\"open".into()))
        );
        assert!(!is_valid_architecture_icon(&ArchitectureIcon::Named(
            "\"open".into()
        )));
        assert_eq!(
            icon_from_field("\""),
            Some(ArchitectureIcon::Named("\"".into()))
        );
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        assert_eq!(parse(""), ArchitectureDocument::empty());
        assert_eq!(parse("architecture-beta"), ArchitectureDocument::empty());
        assert_eq!(
            emit_architecture(&ArchitectureDocument::empty()),
            "architecture-beta"
        );
        // `service`, `group` and `junction` are keywords `merman` 0.8 refuses
        // as ids, so the first id offered for each carries a number.
        for (prefix, expected) in [
            ("service", "service2"),
            ("group", "group2"),
            ("junction", "junction2"),
        ] {
            assert_eq!(ArchitectureDocument::empty().unused_id(prefix), expected);
        }
        let document = parse("architecture-beta\n    service service2\n    group service3");
        assert_eq!(document.unused_id("service"), "service4");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "architecture-beta\r\n    service a(server)[A]  \r\n    service b  \r\n    a:R -- L:b  \r\n";
        let document = parse(source);
        assert_eq!(
            emit_architecture(&document),
            "architecture-beta\n    service a(server)[A]\n    service b\n    a:R -- L:b"
        );
    }

    /// Every architecture example `merman` ships: each one the editor opens is
    /// written back as the same diagram, stably, and still draws.
    #[test]
    fn merman_examples_round_trip() {
        let Some(fixtures) = crate::merman_fixture_directory("architecture") else {
            return;
        };
        let mut paths = std::fs::read_dir(fixtures)
            .expect("the fixture directory")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|extension| extension == "mmd"))
            .collect::<Vec<_>>();
        paths.sort();
        assert!(paths.len() > 100, "found only {} examples", paths.len());

        let mut opened = 0;
        let mut merman_refused = 0;
        let mut refused: Vec<(String, String)> = Vec::new();
        for path in &paths {
            let source = std::fs::read_to_string(path).expect("a readable example");
            if merman_model(&source).is_none() {
                merman_refused += 1;
                assert!(parse_architecture(&source).is_err(), "{path:?}");
                continue;
            }
            let document = match parse_architecture(&source) {
                Ok(document) => document,
                Err(error) => {
                    refused.push((format!("{path:?}"), format!("{error}")));
                    continue;
                }
            };
            opened += 1;
            let emitted = emit_architecture(&document);
            let reparsed = parse_architecture(&emitted).unwrap_or_else(|error| {
                panic!("{path:?} written back does not open: {error}\n{emitted}")
            });
            assert_eq!(reparsed, document, "{path:?}");
            assert_eq!(emit_architecture(&reparsed), emitted, "{path:?}");
            assert_same_diagram(&source, &emitted);
            if render_to_svg(&source, &MermaidTheme::default()).is_ok() {
                assert_renders(&emitted);
            }
        }
        eprintln!(
            "architecture examples: {} opened, {} refused by merman, {} refused by the editor",
            opened,
            merman_refused,
            refused.len()
        );
        for (path, reason) in &refused {
            eprintln!("  {path}: {reason}");
        }
        assert!(
            refused.len() * 20 <= opened,
            "the editor refuses too many examples merman reads: {refused:#?}"
        );
    }
}
