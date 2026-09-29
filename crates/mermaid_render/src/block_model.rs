//! A round-trippable editing model for Mermaid block diagrams (`block-beta`).
//!
//! [`parse_block`] reads the body of a ```` ```mermaid ```` fence into
//! [`BlockDocument`], and [`emit_block`] writes it back. See
//! [`crate::mermaid_model`] for the invariants every diagram kind keeps.
//!
//! `merman` lays a block diagram out from the order its blocks are written in:
//! each container (the diagram, or a `block … end` group) fills its `columns`
//! left to right and top to bottom, and a block goes where it is *first*
//! mentioned. The model therefore keeps the blocks as an outline in source
//! order ([`BlockDocument::entries`]), and keeps everything that only refers to
//! blocks — edges, `classDef`, `class`, `style` — in a list of its own
//! ([`BlockDocument::statements`]) that is written after every block. That is
//! safe because by then every block they name has been placed, and it is
//! necessary because `merman` places a block at an edge, or leaves it out of
//! the layout after a `class` or `style`, when that is where it is first named.
//!
//! What the parser rewrites, all of it stable from the first emit:
//!
//! - An edge that is the first mention of a block becomes the block's entry at
//!   that place (written as a bare block on the edge's line), and the edge
//!   itself names the block by id. An edge that gives an already placed block a
//!   shape is folded into that block, the way `merman` merges it.
//! - Statements between blocks move below the blocks, in their order. A
//!   comment goes with them once one of them has been read at the top level.
//! - `columns` is written after the comments that open its container.
//! - A `block` line followed by blocks on the same line is split.
//! - Indentation, the spacing inside a statement, a group's brackets (only its
//!   label is drawn) and the marker before an edge label (`==`, `-.`), which
//!   `merman` reads but does not keep.
//!
//! What is refused, with the reason: anything `merman` itself rejects, a block
//! placed twice (Mermaid ignores the second), a `class` or `style` written
//! before the block it names (Mermaid would then leave the block out of the
//! layout), `%%` comments after a statement, ids the emitter could not write
//! back unambiguously, and bracket pairs Mermaid draws as a plain box.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, keyword_tail, scan_preamble};
use crate::mindmap_model::{
    OutlineItem, outline_move, outline_parent, outline_previous_sibling, outline_subtree_end,
};

const HEADERS: &[&str] = &["block-beta", "block"];

/// Words `merman` reads as a statement wherever a block id could start, and
/// `root`, the id `merman` gives the diagram itself.
const RESERVED_IDS: &[&str] = &[
    "end", "space", "columns", "classDef", "class", "style", "block", "root",
];

/// The keyword the diagram opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKeyword {
    BlockBeta,
    Block,
}

impl BlockKeyword {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::BlockBeta => "block-beta",
            Self::Block => "block",
        }
    }
}

/// How many columns a container lays its blocks out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockColumns {
    /// `columns auto`: one row, which is also what a container without a
    /// `columns` statement gets.
    Auto,
    Count(u32),
}

impl BlockColumns {
    /// The text after `columns`.
    pub fn text(self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Count(count) => count.to_string(),
        }
    }

    /// Reads what follows `columns`, or `None` when it is not a column count
    /// the form writes.
    pub fn from_text(text: &str) -> Option<Self> {
        let text = text.trim();
        if text == "auto" {
            return Some(Self::Auto);
        }
        parse_positive(text).map(Self::Count)
    }

    fn is_valid(self) -> bool {
        self != Self::Count(0)
    }
}

fn parse_positive(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().filter(|value| *value > 0)
}

/// The shape a block is drawn in, named by the brackets around its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockShape {
    Square,
    Round,
    Stadium,
    Subroutine,
    Cylinder,
    Circle,
    DoubleCircle,
    Asymmetric,
    Diamond,
    Hexagon,
    LeanRight,
    LeanLeft,
    Trapezoid,
    InvertedTrapezoid,
}

impl BlockShape {
    pub const ALL: [BlockShape; 14] = [
        BlockShape::Square,
        BlockShape::Round,
        BlockShape::Stadium,
        BlockShape::Subroutine,
        BlockShape::Cylinder,
        BlockShape::Circle,
        BlockShape::DoubleCircle,
        BlockShape::Asymmetric,
        BlockShape::Diamond,
        BlockShape::Hexagon,
        BlockShape::LeanRight,
        BlockShape::LeanLeft,
        BlockShape::Trapezoid,
        BlockShape::InvertedTrapezoid,
    ];

    /// The brackets written before and after the quoted label.
    pub fn delimiters(self) -> (&'static str, &'static str) {
        match self {
            Self::Square => ("[", "]"),
            Self::Round => ("(", ")"),
            Self::Stadium => ("([", "])"),
            Self::Subroutine => ("[[", "]]"),
            Self::Cylinder => ("[(", ")]"),
            Self::Circle => ("((", "))"),
            Self::DoubleCircle => ("(((", ")))"),
            Self::Asymmetric => (">", "]"),
            Self::Diamond => ("{", "}"),
            Self::Hexagon => ("{{", "}}"),
            Self::LeanRight => ("[/", "/]"),
            Self::LeanLeft => ("[\\", "\\]"),
            Self::Trapezoid => ("[/", "\\]"),
            Self::InvertedTrapezoid => ("[\\", "/]"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            // `tr` rather than `t!`: the brackets would be read as placeholders.
            Self::Square => i18n::tr(i18n::msg!("Square [ ]")),
            Self::Round => i18n::tr(i18n::msg!("Rounded ( )")),
            Self::Stadium => i18n::tr(i18n::msg!("Stadium ([ ])")),
            Self::Subroutine => i18n::tr(i18n::msg!("Subroutine [[ ]]")),
            Self::Cylinder => i18n::tr(i18n::msg!("Cylinder [( )]")),
            Self::Circle => i18n::tr(i18n::msg!("Circle (( ))")),
            Self::DoubleCircle => i18n::tr(i18n::msg!("Double Circle ((( )))")),
            Self::Asymmetric => i18n::tr(i18n::msg!("Flag > ]")),
            Self::Diamond => i18n::tr(i18n::msg!("Diamond { }")),
            Self::Hexagon => i18n::tr(i18n::msg!("Hexagon {{ }}")),
            Self::LeanRight => i18n::tr(i18n::msg!("Lean Right [/ /]")),
            Self::LeanLeft => i18n::tr(i18n::msg!("Lean Left [\\ \\]")),
            Self::Trapezoid => i18n::tr(i18n::msg!("Trapezoid [/ \\]")),
            Self::InvertedTrapezoid => i18n::tr(i18n::msg!("Inverted Trapezoid [\\ /]")),
        }
    }

    /// The shape `merman` gives the bracket pair `start` + `end`, or `None`
    /// for a pair it reads but draws as a plain box.
    fn from_delimiters(start: &str, end: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|shape| shape.delimiters() == (start, end))
    }
}

/// One of the directions an arrow block points in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockArrowDirection {
    Right,
    Left,
    Up,
    Down,
    X,
    Y,
}

impl BlockArrowDirection {
    pub const ALL: [BlockArrowDirection; 6] = [
        BlockArrowDirection::Right,
        BlockArrowDirection::Left,
        BlockArrowDirection::Up,
        BlockArrowDirection::Down,
        BlockArrowDirection::X,
        BlockArrowDirection::Y,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Left => "left",
            Self::Up => "up",
            Self::Down => "down",
            Self::X => "x",
            Self::Y => "y",
        }
    }

    fn from_keyword(word: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|direction| direction.keyword() == word)
    }
}

/// Reads a comma separated direction list such as `x, down`.
pub fn parse_arrow_directions(text: &str) -> Option<Vec<BlockArrowDirection>> {
    let directions = text
        .split(',')
        .map(|word| BlockArrowDirection::from_keyword(word.trim()))
        .collect::<Option<Vec<_>>>()?;
    (!directions.is_empty()).then_some(directions)
}

/// Writes a direction list the way [`parse_arrow_directions`] reads it.
pub fn arrow_directions_text(directions: &[BlockArrowDirection]) -> String {
    directions
        .iter()
        .map(|direction| direction.keyword())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockNode {
    pub id: String,
    /// `None` for a bare id, which Mermaid draws as a box labelled with the id.
    pub shape: Option<BlockShape>,
    /// The text between the quotes, backticks included for a Markdown label.
    /// Only written when there is a shape.
    pub label: String,
    /// How many columns the block spans (`id:2`); `None` for one.
    pub width: Option<u32>,
}

/// A block drawn as a fat arrow: `id<["label"]>(right)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockArrow {
    pub id: String,
    pub label: String,
    pub directions: Vec<BlockArrowDirection>,
    pub width: Option<u32>,
}

/// A `block … end` group. Its children are the entries after it that are one
/// level deeper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockGroup {
    /// `None` for an anonymous `block`, which nothing can refer to and which can
    /// have neither a label nor a width.
    pub id: Option<String>,
    pub label: Option<String>,
    pub width: Option<u32>,
    pub columns: Option<BlockColumns>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockItem {
    Node(BlockNode),
    Arrow(BlockArrow),
    /// An empty cell, `space` or `space:3`.
    Space {
        width: Option<u32>,
    },
    Group(BlockGroup),
    /// A `%%` line, kept verbatim (trimmed) where it was written.
    Comment(String),
}

impl BlockItem {
    /// The id other statements can name the item by.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Node(node) => Some(&node.id),
            Self::Arrow(arrow) => Some(&arrow.id),
            Self::Group(group) => group.id.as_deref(),
            Self::Space { .. } | Self::Comment(_) => None,
        }
    }

    fn id_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Node(node) => Some(&mut node.id),
            Self::Arrow(arrow) => Some(&mut arrow.id),
            Self::Group(group) => group.id.as_mut(),
            Self::Space { .. } | Self::Comment(_) => None,
        }
    }
}

/// One row of the block outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEntry {
    /// 0 for the diagram's own blocks, one more for each group it is inside.
    pub depth: usize,
    /// Written on the same line as the entry before it. Only a matter of
    /// layout in the text: Mermaid reads `a b` and `a` + `b` alike.
    pub joined: bool,
    pub item: BlockItem,
}

impl BlockEntry {
    pub fn new(depth: usize, item: BlockItem) -> Self {
        Self {
            depth,
            joined: false,
            item,
        }
    }
}

impl OutlineItem for BlockEntry {
    fn depth(&self) -> usize {
        self.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }
}

/// `a --> b` or `a -- "label" --> b`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEdge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    /// The link as written, `-->` / `---` / `--x` / `--o` and their longer or
    /// thicker spellings. Mermaid only keeps the head at its end.
    pub link: String,
}

/// The links the form offers, with what they draw. The names are English
/// msgids: translate them with `i18n::tr` where they are shown.
pub const BLOCK_LINKS: [(&str, &str); 4] = [
    ("-->", i18n::msg!("Arrow")),
    ("---", i18n::msg!("Line Only")),
    ("--x", i18n::msg!("Cross")),
    ("--o", i18n::msg!("Circle")),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockStatement {
    Edge(BlockEdge),
    /// `classDef name css`.
    ClassDef {
        name: String,
        css: String,
    },
    /// `class a,b name`.
    Class {
        targets: Vec<String>,
        class_name: String,
    },
    /// `style id css`.
    Style {
        target: String,
        css: String,
    },
    Comment(String),
}

/// An editable block diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockDocument {
    pub keyword: BlockKeyword,
    /// The diagram's own `columns`.
    pub columns: Option<BlockColumns>,
    pub entries: Vec<BlockEntry>,
    pub statements: Vec<BlockStatement>,
    /// Frontmatter, directives and comments before the keyword.
    pub preserved_lines: Vec<PreservedLine>,
}

impl BlockDocument {
    /// A blank block diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: BlockKeyword::BlockBeta,
            columns: None,
            entries: Vec::new(),
            statements: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every id a block or a group is declared with, in outline order.
    pub fn ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| entry.item.id())
            .map(str::to_string)
            .collect()
    }

    /// An id no block or group uses, based on `prefix`.
    pub fn unused_id(&self, prefix: &str) -> String {
        let ids = self.ids();
        if !ids.iter().any(|id| id == prefix) {
            return prefix.to_string();
        }
        (1..=ids.len() + 1)
            .map(|suffix| format!("{prefix}{suffix}"))
            .find(|candidate| !ids.contains(candidate))
            .unwrap_or_else(|| prefix.to_string())
    }

    /// Renames every id in `renames` (old, new) at once, in the blocks and in
    /// every statement that names them.
    pub fn rename_ids(&mut self, renames: &[(String, String)]) {
        for entry in &mut self.entries {
            if let Some(id) = entry.item.id_mut()
                && let Some((_, new)) = renames.iter().find(|(old, _)| old == id)
            {
                *id = new.clone();
            }
        }
        self.rename_references(renames);
    }

    /// Points every statement that names an id in `renames` (old, new) at the
    /// new spelling. The blocks themselves are left alone, for the form, whose
    /// id fields already hold the new spellings.
    pub fn rename_references(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let renamed = |id: &mut String| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == id) {
                *id = new.clone();
            }
        };
        for statement in &mut self.statements {
            match statement {
                BlockStatement::Edge(edge) => {
                    renamed(&mut edge.from);
                    renamed(&mut edge.to);
                }
                BlockStatement::Class { targets, .. } => targets.iter_mut().for_each(renamed),
                BlockStatement::Style { target, .. } => renamed(target),
                BlockStatement::ClassDef { .. } | BlockStatement::Comment(_) => {}
            }
        }
    }

    /// Takes `ids` out of every statement: an edge or a `style` naming one goes,
    /// a `class` loses it from its list and goes once the list is empty.
    /// Returns the indices the removed statements had, in order.
    pub fn remove_references(&mut self, ids: &[String]) -> Vec<usize> {
        let gone = |id: &str| ids.iter().any(|removed| removed == id);
        let mut removed = Vec::new();
        let mut index = 0;
        self.statements.retain_mut(|statement| {
            let keep = match statement {
                BlockStatement::Edge(edge) => !gone(&edge.from) && !gone(&edge.to),
                BlockStatement::Style { target, .. } => !gone(target),
                BlockStatement::Class { targets, .. } => {
                    targets.retain(|target| !gone(target));
                    !targets.is_empty()
                }
                BlockStatement::ClassDef { .. } | BlockStatement::Comment(_) => true,
            };
            if !keep {
                removed.push(index);
            }
            index += 1;
            keep
        });
        removed
    }

    /// Removes the entry at `index` with everything inside it, and every
    /// statement that would be left naming a removed id (see
    /// [`Self::remove_references`]). Returns the indices of the statements that
    /// went, or `None` when there is no such entry.
    pub fn remove_entry(&mut self, index: usize) -> Option<Vec<usize>> {
        if index >= self.entries.len() {
            return None;
        }
        let end = outline_subtree_end(&self.entries, index);
        let removed = self
            .entries
            .drain(index..end)
            .filter_map(|entry| entry.item.id().map(str::to_string))
            .collect::<Vec<_>>();
        Some(self.remove_references(&removed))
    }

    /// Swaps the entry at `index`, with its children, with the sibling above
    /// or below it. Returns where it went.
    pub fn move_entry(&mut self, index: usize, forward: bool) -> Option<usize> {
        outline_move(&mut self.entries, index, forward)
    }

    /// Whether the entry at `index` can go into the group right above it.
    pub fn can_indent(&self, index: usize) -> bool {
        block_can_indent(&self.entries, index)
    }

    pub fn indent_entry(&mut self, index: usize) -> bool {
        block_indent(&mut self.entries, index)
    }

    pub fn outdent_entry(&mut self, index: usize) -> Option<usize> {
        block_outdent(&mut self.entries, index)
    }

    /// Turns the entry at `index` into an item of `kind`, keeping what the two
    /// have in common. A group that stops being one hands its children to its
    /// own parent.
    pub fn convert_entry(&mut self, index: usize, kind: BlockItemKind) -> bool {
        let Some(item) = self.entries.get(index).map(|entry| entry.item.clone()) else {
            return false;
        };
        let fallback = self.unused_id(kind.id_prefix());
        let converted = convert_block_item(&item, kind, fallback);
        if matches!(item, BlockItem::Group(_)) && kind != BlockItemKind::Group {
            block_release_children(&mut self.entries, index);
        }
        if let Some(entry) = self.entries.get_mut(index) {
            entry.item = converted;
        }
        true
    }

    /// Why the document cannot be written back as a diagram Mermaid draws the
    /// way the form shows it, in Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(error) = self.item_and_statement_error() {
            return Some(error);
        }
        if html_attributes_rewritten(&emit_block(self)) {
            return Some(html_attributes_rewritten_message().to_string());
        }
        None
    }

    fn item_and_statement_error(&self) -> Option<String> {
        if self.columns.is_some_and(|columns| !columns.is_valid()) {
            return Some(
                i18n::t!("The number of columns must be a whole number of 1 or more, or auto.")
                    .to_string(),
            );
        }

        let mut ids: Vec<&str> = Vec::new();
        let mut previous: Option<(usize, bool)> = None;
        for entry in &self.entries {
            if entry.depth > allowed_depth(previous) {
                return Some(
                    i18n::t!(
                        "An item outside any group is indented. Use “Move into the Group Above” to put it into a group."
                    )
                    .to_string(),
                );
            }
            previous = Some((entry.depth, matches!(entry.item, BlockItem::Group(_))));

            if let Some(error) = item_error(&entry.item) {
                return Some(error);
            }
            if let Some(id) = entry.item.id() {
                if ids.contains(&id) {
                    return Some(i18n::t!(
                        "The ID `{id}` is used twice. Mermaid does not place the second one, so give it a different ID."
                    ));
                }
                ids.push(id);
            }
        }

        let declared = |id: &str| ids.contains(&id);
        for statement in &self.statements {
            match statement {
                BlockStatement::Edge(edge) => {
                    for end in [&edge.from, &edge.to] {
                        if !declared(end) {
                            return Some(i18n::t!(
                                "There is no block `{end}` for the edge to connect (written only in an edge, Mermaid places a new block there)."
                            ));
                        }
                    }
                    if !is_valid_block_link(&edge.link) {
                        return Some(i18n::t!("The edge line `{}` cannot be written.", edge.link));
                    }
                    if let Some(label) = &edge.label
                        && !is_valid_edge_label(label)
                    {
                        return Some(i18n::t!(
                            "The edge label `{label}` cannot be written (`\"` and line breaks are not allowed)."
                        ));
                    }
                }
                BlockStatement::ClassDef { name, css } => {
                    if !is_valid_block_class_name(name) {
                        return Some(i18n::t!(
                            "The classDef name `{name}` cannot be used (only ASCII letters, digits and `_`, and it cannot be empty)."
                        ));
                    }
                    if !is_valid_block_css(css) {
                        return Some(i18n::t!(
                            "The style of classDef `{name}` cannot contain line breaks."
                        ));
                    }
                }
                BlockStatement::Class {
                    targets,
                    class_name,
                } => {
                    if targets.is_empty() {
                        return Some(i18n::t!(
                            "Write at least one block to give the class `{class_name}` to."
                        ));
                    }
                    for target in targets {
                        if !declared(target) {
                            return Some(i18n::t!(
                                "There is no block `{target}` to give the class `{class_name}` to."
                            ));
                        }
                    }
                    if !is_valid_block_class_name(class_name) {
                        return Some(i18n::t!(
                            "The class name `{class_name}` cannot be used (only ASCII letters, digits and `_`, and it cannot be empty)."
                        ));
                    }
                }
                BlockStatement::Style { target, css } => {
                    if !declared(target) {
                        return Some(i18n::t!(
                            "There is no block `{target}` to give the style to."
                        ));
                    }
                    if !is_valid_block_css(css) {
                        return Some(i18n::t!(
                            "The style of `{target}` cannot contain line breaks."
                        ));
                    }
                }
                BlockStatement::Comment(text) => {
                    if !is_valid_block_comment(text) {
                        return Some(
                            i18n::t!("A comment must be a single line that starts with `%%`.")
                                .to_string(),
                        );
                    }
                }
            }
        }
        None
    }
}

/// The kinds of item the form's kind menu switches between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockItemKind {
    Node,
    Arrow,
    Space,
    Group,
}

impl BlockItemKind {
    pub const ALL: [BlockItemKind; 4] = [
        BlockItemKind::Node,
        BlockItemKind::Arrow,
        BlockItemKind::Space,
        BlockItemKind::Group,
    ];

    pub fn of(item: &BlockItem) -> Option<Self> {
        match item {
            BlockItem::Node(_) => Some(Self::Node),
            BlockItem::Arrow(_) => Some(Self::Arrow),
            BlockItem::Space { .. } => Some(Self::Space),
            BlockItem::Group(_) => Some(Self::Group),
            BlockItem::Comment(_) => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Node => i18n::t!("Block"),
            Self::Arrow => i18n::t!("Arrow"),
            Self::Space => i18n::t!("Space"),
            Self::Group => i18n::t!("Group"),
        }
    }

    /// What a new id of this kind is based on.
    pub fn id_prefix(self) -> &'static str {
        match self {
            // `block` itself is a keyword, so it cannot be an id.
            Self::Node | Self::Space => "node",
            Self::Arrow => "arrow",
            Self::Group => "group",
        }
    }
}

/// `item` as an item of `kind`, keeping its id, label and width where the new
/// kind has them. `fallback_id` is used when `item` has no id to keep.
pub fn convert_block_item(item: &BlockItem, kind: BlockItemKind, fallback_id: String) -> BlockItem {
    if BlockItemKind::of(item) == Some(kind) {
        return item.clone();
    }
    let id = item.id().map(str::to_string).unwrap_or(fallback_id);
    let (label, width) = match item {
        BlockItem::Node(node) => (node.shape.map(|_| node.label.clone()), node.width),
        BlockItem::Arrow(arrow) => (Some(arrow.label.clone()), arrow.width),
        BlockItem::Space { width } => (None, *width),
        BlockItem::Group(group) => (group.label.clone(), group.width),
        BlockItem::Comment(_) => (None, None),
    };
    match kind {
        BlockItemKind::Node => BlockItem::Node(BlockNode {
            id,
            shape: label.is_some().then_some(BlockShape::Square),
            label: label.unwrap_or_default(),
            width,
        }),
        BlockItemKind::Arrow => BlockItem::Arrow(BlockArrow {
            id,
            label: label.unwrap_or_default(),
            directions: vec![BlockArrowDirection::Right],
            width,
        }),
        BlockItemKind::Space => BlockItem::Space { width },
        BlockItemKind::Group => BlockItem::Group(BlockGroup {
            id: Some(id),
            label,
            width,
            columns: None,
        }),
    }
}

/// An outline row that may be a group, which is the only kind of row other
/// rows can sit inside.
pub trait BlockOutlineItem: OutlineItem {
    fn is_group(&self) -> bool;
}

impl BlockOutlineItem for BlockEntry {
    fn is_group(&self) -> bool {
        matches!(self.item, BlockItem::Group(_))
    }
}

/// Whether the row at `index` can go into the group right above it.
pub fn block_can_indent<T: BlockOutlineItem>(items: &[T], index: usize) -> bool {
    outline_previous_sibling(items, index)
        .and_then(|previous| items.get(previous))
        .is_some_and(BlockOutlineItem::is_group)
}

/// Moves the row at `index` into the group right above it, as its last child.
/// It already sits right after that group's children, so only the depths
/// change.
pub fn block_indent<T: BlockOutlineItem>(items: &mut [T], index: usize) -> bool {
    if !block_can_indent(items, index) {
        return false;
    }
    let end = outline_subtree_end(items, index);
    let Some(subtree) = items.get_mut(index..end) else {
        return false;
    };
    for item in subtree {
        item.set_depth(item.depth() + 1);
    }
    true
}

/// Moves the row at `index` out of its group, right after the group. Returns
/// where it went.
pub fn block_outdent<T: BlockOutlineItem>(items: &mut Vec<T>, index: usize) -> Option<usize> {
    if items.get(index)?.depth() == 0 {
        return None;
    }
    let parent = outline_parent(items, index)?;
    let end = outline_subtree_end(items, index);
    let mut subtree = items.drain(index..end).collect::<Vec<_>>();
    for item in &mut subtree {
        item.set_depth(item.depth().saturating_sub(1));
    }
    let at = outline_subtree_end(items, parent);
    let tail = items.split_off(at);
    items.extend(subtree);
    items.extend(tail);
    Some(at)
}

/// Moves the children of the row at `index` up one level, into the group's own
/// parent, where they stay in place.
pub fn block_release_children<T: BlockOutlineItem>(items: &mut [T], index: usize) {
    let end = outline_subtree_end(items, index);
    if let Some(children) = items.get_mut(index + 1..end) {
        for child in children {
            child.set_depth(child.depth().saturating_sub(1));
        }
    }
}

/// Removes the row at `index` with everything inside it.
pub fn block_remove<T: BlockOutlineItem>(items: &mut Vec<T>, index: usize) -> Vec<T> {
    if index >= items.len() {
        return Vec::new();
    }
    let end = outline_subtree_end(items, index);
    items.drain(index..end).collect()
}

/// The deepest an entry can sit after the entry before it, given as its depth
/// and whether it is a group: inside it when it is one, beside it otherwise.
fn allowed_depth(previous: Option<(usize, bool)>) -> usize {
    match previous {
        None => 0,
        Some((depth, true)) => depth + 1,
        Some((depth, false)) => depth,
    }
}

/// What is wrong with one item on its own, or `None`.
fn item_error(item: &BlockItem) -> Option<String> {
    let width_error = |width: Option<u32>, what: &str| {
        (width == Some(0)).then(|| i18n::t!("The width of {what} must be 1 or more."))
    };
    match item {
        BlockItem::Node(node) => {
            if !is_valid_block_id(&node.id) {
                return Some(invalid_id_message(&node.id));
            }
            if node.shape.is_some() && !is_valid_block_label(&node.label) {
                return Some(i18n::t!(
                    "The label `{label}` of block `{id}` cannot be written (`\"` and line breaks are not allowed, and a label that starts with ` must end with `).",
                    id = node.id,
                    label = node.label
                ));
            }
            width_error(node.width, &i18n::t!("block `{}`", node.id))
        }
        BlockItem::Arrow(arrow) => {
            if !is_valid_block_id(&arrow.id) {
                return Some(invalid_id_message(&arrow.id));
            }
            if !is_valid_arrow_label(&arrow.label) {
                return Some(i18n::t!(
                    "The label `{label}` of arrow `{id}` cannot be written (`\"` and line breaks are not allowed).",
                    id = arrow.id,
                    label = arrow.label
                ));
            }
            if arrow.directions.is_empty() {
                return Some(i18n::t!(
                    "Write at least one direction for arrow `{}` (right / left / up / down / x / y).",
                    arrow.id
                ));
            }
            width_error(arrow.width, &i18n::t!("arrow `{}`", arrow.id))
        }
        BlockItem::Space { width } => width_error(*width, i18n::t!("the space")),
        BlockItem::Group(group) => {
            if group.columns.is_some_and(|columns| !columns.is_valid()) {
                return Some(
                    i18n::t!(
                        "The number of columns of a group must be a whole number of 1 or more, or auto."
                    )
                    .to_string(),
                );
            }
            let Some(id) = &group.id else {
                return (group.label.is_some() || group.width.is_some()).then(|| {
                    i18n::t!("A group without an ID cannot have a label or a width. Give it an ID.")
                        .to_string()
                });
            };
            if !is_valid_block_id(id) {
                return Some(invalid_id_message(id));
            }
            if let Some(label) = &group.label
                && !is_valid_block_label(label)
            {
                return Some(i18n::t!(
                    "The label `{label}` of group `{id}` cannot be written (`\"` and line breaks are not allowed, and a label that starts with ` must end with `)."
                ));
            }
            width_error(group.width, &i18n::t!("group `{id}`"))
        }
        BlockItem::Comment(text) => (!is_valid_block_comment(text)).then(|| {
            i18n::t!("A comment must be a single line that starts with `%%`.").to_string()
        }),
    }
}

fn invalid_id_message(id: &str) -> String {
    i18n::t!(
        "The ID `{id}` cannot be used (spaces and the symbols ( ) [ ] {{ }} < > : - \" ' , % = ~ ; # & | \\ / ` are not allowed, nor are end / space / columns / class / classDef / style / block / root)."
    )
}

/// Whether `id` can be written as a block id and read back as that one id.
///
/// `merman` ends an id at whitespace and at `( [ - ) { } < > :`. The rest of
/// the symbols are refused because they mean something elsewhere: `,`
/// separates the ids of a `class`, `%%` opens a comment, `#…;` is an entity,
/// `=` and `~` would let a line that starts with the id be read as an edge
/// continuing the line above, and quotes and slashes belong to labels and
/// brackets. The reserved words are statements wherever an id could start.
pub fn is_valid_block_id(id: &str) -> bool {
    !id.is_empty()
        && !id.chars().any(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '(' | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                        | ':'
                        | '-'
                        | '"'
                        | '\''
                        | ','
                        | '%'
                        | '='
                        | '~'
                        | ';'
                        | '#'
                        | '&'
                        | '|'
                        | '\\'
                        | '/'
                        | '`'
                )
        })
        && !RESERVED_IDS.contains(&id)
}

/// Whether `label` can be written between the quotes of a shaped block or a
/// group and read back as itself. A label opening with a backtick is read as
/// Markdown up to the next backtick-and-quote, so it has to close with one.
pub fn is_valid_block_label(label: &str) -> bool {
    is_valid_arrow_label(label)
        && (!label.starts_with('`') || (label.len() >= 2 && label.ends_with('`')))
}

/// Whether `label` can be written between the quotes of an arrow block, which
/// Mermaid reads as a plain string up to the next quote.
pub fn is_valid_arrow_label(label: &str) -> bool {
    !label.contains(['"', '\n', '\r']) && !opens_directive(label)
}

pub fn is_valid_edge_label(label: &str) -> bool {
    !label.contains(['"', '\n', '\r']) && !opens_directive(label)
}

/// Whether `name` can be a `classDef` name, and so the class a `class`
/// statement gives: Mermaid reads it as a word, ASCII letters, digits and `_`
/// (`merman` 0.8 refuses anything else).
pub fn is_valid_block_class_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether `css` can follow a `style` target or a `classDef` name. Mermaid takes
/// the rest of the line, so only a line break is in the way.
pub fn is_valid_block_css(css: &str) -> bool {
    css.trim() == css && !css.contains(['\n', '\r']) && !opens_directive(css)
}

/// Whether `text` can be written as a comment line. A `%%{ … }%%` directive
/// has to be the whole line, or Mermaid reads what follows it as statements
/// (or, left open, cuts the rest of the diagram).
pub fn is_valid_block_comment(text: &str) -> bool {
    if !(text.starts_with("%%") && text.trim() == text && !text.contains(['\n', '\r'])) {
        return false;
    }
    let unclosed = text.rfind("%%{").is_some_and(|start| {
        !text
            .get(start + 3..)
            .is_some_and(|rest| rest.contains("}%%"))
    });
    !unclosed && text_after_directive(text).is_none_or(str::is_empty)
}

/// Mermaid cuts `%%{ … }%%` out of the whole diagram before reading it, even
/// out of a label, and an unclosed `%%{` takes the rest of the diagram with it.
fn opens_directive(text: &str) -> bool {
    text.contains("%%{")
}

/// What follows the `}%%` that closes the directive `line` starts with, or
/// `None` when it does not start with a closed one.
fn text_after_directive(line: &str) -> Option<&str> {
    let body = line.strip_prefix("%%{")?;
    let end = body.find("}%%")?;
    body.get(end + 3..)
}

/// Whether Mermaid's preprocessing would rewrite `text` before reading it:
/// inside anything shaped like an HTML tag (`<b … >`, which can span several
/// labels), it turns `="…"` into `='…'`, taking the quotes of the labels in
/// between with it.
fn html_attributes_rewritten(text: &str) -> bool {
    let bytes = text.as_bytes();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut probe = 0;
    while let Some(start) = text.get(probe..).and_then(|rest| rest.find('<')) {
        let tag_start = probe + start + 1;
        if !bytes.get(tag_start).copied().is_some_and(is_word) {
            probe = tag_start;
            continue;
        }
        let mut tag_end = tag_start + 1;
        while bytes.get(tag_end).copied().is_some_and(is_word) {
            tag_end += 1;
        }
        let Some(end) = text.get(tag_end..).and_then(|rest| rest.find('>')) else {
            probe = tag_start;
            continue;
        };
        let attributes = text.get(tag_end..tag_end + end).unwrap_or_default();
        if let Some(value) = attributes.find("=\"")
            && attributes
                .get(value + 2..)
                .is_some_and(|rest| rest.contains('"'))
        {
            return true;
        }
        probe = tag_end + end + 1;
    }
    false
}

fn html_attributes_rewritten_message() -> &'static str {
    i18n::t!(
        "When text shaped like `<tag …>` contains `=\"…\"`, Mermaid rewrites the `\"` to `'` before reading it, which changes the diagram. Change the `<` of the label or the `=` at its end."
    )
}

/// Reads a width field: empty for one column, or a positive integer. `None`
/// when the text is neither.
pub fn parse_block_width(text: &str) -> Option<Option<u32>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(None);
    }
    parse_positive(text).map(Some)
}

/// Whether `link` is a link token `merman` reads, checked the way its block
/// parser checks one.
pub fn is_valid_block_link(link: &str) -> bool {
    if link.is_empty() || link.chars().any(char::is_whitespace) {
        return false;
    }
    if link.chars().all(|character| character == '~') {
        return link.len() >= 3;
    }
    let rest = link.strip_prefix(['x', 'o', '<']).unwrap_or(link);
    is_valid_repeated_link(rest, '-')
        || is_valid_repeated_link(rest, '=')
        || is_valid_dotted_link(rest)
}

fn is_valid_repeated_link(rest: &str, stroke: char) -> bool {
    if !rest.starts_with(stroke) {
        return false;
    }
    if rest.chars().all(|character| character == stroke) {
        return rest.chars().count() >= 3;
    }
    let mut characters = rest.chars();
    let Some(last) = characters.next_back() else {
        return false;
    };
    let body = characters.as_str();
    (last == stroke || matches!(last, 'x' | 'o' | '>'))
        && body.chars().count() >= 2
        && body.chars().all(|character| character == stroke)
}

fn is_valid_dotted_link(rest: &str) -> bool {
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let after_dots = rest.trim_start_matches('.');
    if after_dots.len() == rest.len() {
        return false;
    }
    let Some(tail) = after_dots.strip_prefix('-') else {
        return false;
    };
    matches!(tail, "" | "x" | "o" | ">")
}

/// Whether `source` is a block diagram: its first statement is `block-beta` or `block`.
pub fn is_block_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_block(source: &str) -> Result<BlockDocument> {
    let document = scan_block(source)?;
    if html_attributes_rewritten(source) {
        bail!(html_attributes_rewritten_message());
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
        if !matches!(parsed.model(), RenderSemanticModel::Block(_)) {
            bail!(i18n::t!("This diagram is not a block diagram."));
        }
    }

    Ok(document)
}

/// What an edge end or a block on a line was written as.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedBlock {
    Node {
        id: String,
        shape: Option<(BlockShape, String)>,
    },
    Arrow {
        id: String,
        label: String,
        directions: Vec<BlockArrowDirection>,
    },
}

impl ParsedBlock {
    fn id(&self) -> &str {
        match self {
            Self::Node { id, .. } | Self::Arrow { id, .. } => id,
        }
    }

    fn into_item(self, width: Option<u32>) -> BlockItem {
        match self {
            Self::Node { id, shape } => {
                let (shape, label) = match shape {
                    Some((shape, label)) => (Some(shape), label),
                    None => (None, String::new()),
                };
                BlockItem::Node(BlockNode {
                    id,
                    shape,
                    label,
                    width,
                })
            }
            Self::Arrow {
                id,
                label,
                directions,
            } => BlockItem::Arrow(BlockArrow {
                id,
                label,
                directions,
                width,
            }),
        }
    }
}

/// One statement of a line of blocks.
enum LineToken {
    Block(ParsedBlock, Option<u32>),
    Space(Option<u32>),
    Edge {
        from: ParsedBlock,
        to: ParsedBlock,
        label: Option<String>,
        link: String,
    },
}

struct Scanner {
    document: BlockDocument,
    /// Index in `entries` of each declared id.
    declared: HashMap<String, usize>,
    /// The groups open around the line being read, as indices into `entries`.
    open_groups: Vec<usize>,
    /// Whether the last statement ended with a block `merman` goes on to read
    /// a link after, past the end of its line.
    link_may_follow: bool,
}

impl Scanner {
    fn depth(&self) -> usize {
        self.open_groups.len()
    }

    fn push_entry(&mut self, joined: bool, item: BlockItem, line: &str) -> Result<()> {
        if let Some(id) = item.id() {
            if self.declared.contains_key(id) {
                bail!(i18n::t!(
                    "The block `{id}` is placed twice ({line}). Mermaid ignores the second one, so merge them into one, then open the editor again."
                ));
            }
            self.declared
                .insert(id.to_string(), self.document.entries.len());
        }
        let depth = self.depth();
        self.document.entries.push(BlockEntry {
            depth,
            joined,
            item,
        });
        Ok(())
    }

    fn columns_mut(&mut self) -> &mut Option<BlockColumns> {
        let group = self
            .open_groups
            .last()
            .and_then(|index| self.document.entries.get_mut(*index))
            .and_then(|entry| match &mut entry.item {
                BlockItem::Group(group) => Some(&mut group.columns),
                _ => None,
            });
        match group {
            Some(columns) => columns,
            None => &mut self.document.columns,
        }
    }

    /// Reads a line of blocks, spaces and edges into the current container.
    fn read_items(&mut self, text: &str, line: &str) -> Result<()> {
        let (tokens, link_may_follow) = parse_line_tokens(text, line)?;
        self.link_may_follow = link_may_follow;
        let mut first = true;
        for token in tokens {
            match token {
                LineToken::Block(block, width) => {
                    self.push_entry(!first, block.into_item(width), line)?;
                    first = false;
                }
                LineToken::Space(width) => {
                    self.push_entry(!first, BlockItem::Space { width }, line)?;
                    first = false;
                }
                LineToken::Edge {
                    from,
                    to,
                    label,
                    link,
                } => {
                    let from_id = from.id().to_string();
                    let to_id = to.id().to_string();
                    for end in [from, to] {
                        if self.mention(end, !first, line)? {
                            first = false;
                        }
                    }
                    self.document
                        .statements
                        .push(BlockStatement::Edge(BlockEdge {
                            from: from_id,
                            to: to_id,
                            label,
                            link,
                        }));
                }
            }
        }
        Ok(())
    }

    /// Reads an edge end: the first mention of an id places the block there,
    /// and a later one that gives it a shape is merged into it the way `merman`
    /// merges it. Returns whether an entry was added.
    fn mention(&mut self, block: ParsedBlock, joined: bool, line: &str) -> Result<bool> {
        let Some(index) = self.declared.get(block.id()).copied() else {
            self.push_entry(joined, block.into_item(None), line)?;
            return Ok(true);
        };
        let Some(entry) = self.document.entries.get_mut(index) else {
            return Ok(false);
        };
        match (block, &mut entry.item) {
            (ParsedBlock::Node { shape: None, .. }, _) => {}
            (
                ParsedBlock::Node {
                    id,
                    shape: Some((shape, label)),
                },
                BlockItem::Node(node),
            ) => {
                // Mermaid takes the later brackets, and the later label unless
                // it is the id itself, which a bare block is labelled with
                // anyway.
                if label != id || node.shape.is_none() {
                    node.label = label;
                }
                node.shape = Some(shape);
            }
            (
                ParsedBlock::Arrow {
                    label, directions, ..
                },
                BlockItem::Arrow(arrow),
            ) if label == arrow.label && directions == arrow.directions => {}
            (block, _) => {
                bail!(i18n::t!(
                    "The shape of `{}` is changed inside an edge ({line}). Write the shape where the block is placed.",
                    block.id()
                ));
            }
        }
        Ok(false)
    }

    fn require_declared(&self, id: &str, line: &str) -> Result<()> {
        if !self.declared.contains_key(id) {
            bail!(i18n::t!(
                "The `{id}` that `{line}` refers to is not placed before this line. Mermaid then does not place `{id}`, so write this after the line that places the block."
            ));
        }
        Ok(())
    }
}

fn scan_block(source: &str) -> Result<BlockDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut scanner = Scanner {
        document: BlockDocument::empty(),
        declared: HashMap::new(),
        open_groups: Vec::new(),
        link_may_follow: false,
    };
    scanner.document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(scanner.document);
    };
    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let (keyword, tail) = if let Some(tail) = keyword_tail(header, "block-beta") {
        (BlockKeyword::BlockBeta, tail)
    } else if let Some(tail) = keyword_tail(header, "block") {
        (BlockKeyword::Block, tail)
    } else {
        bail!(i18n::t!(
            "This is not a block diagram (only a diagram that starts with `block-beta` can be edited in the structured editor)."
        ));
    };
    if !tail.is_empty() {
        bail!(i18n::t!(
            "The structured editor does not support `{tail}` on the `{}` line. Write it on the next line.",
            keyword.keyword()
        ));
    }
    scanner.document.keyword = keyword;

    for line in lines.iter().skip(header_index + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                bail!(i18n::t!(
                    "The structured editor does not support a `%%{{ … }}%%` that spans several lines. Write it on one line at the top of the diagram."
                ));
            }
            // Mermaid cuts a directive out of the line and reads what follows
            // it as statements, which a comment row would move and hide.
            if let Some(rest) = text_after_directive(trimmed)
                && !rest.trim().is_empty()
            {
                bail!(i18n::t!(
                    "The structured editor does not support `{}` written right after `%%{{ … }}%%`. Write it on the next line.",
                    rest.trim()
                ));
            }
            let comment = trimmed.to_string();
            if scanner.depth() == 0 && !scanner.document.statements.is_empty() {
                scanner
                    .document
                    .statements
                    .push(BlockStatement::Comment(comment));
            } else {
                scanner.push_entry(false, BlockItem::Comment(comment), trimmed)?;
            }
            continue;
        }
        scan_statement(&mut scanner, trimmed)?;
    }
    // `merman` 0.7 closed a group left open at the end of the fence; 0.8
    // refuses it.
    if !scanner.open_groups.is_empty() {
        bail!(i18n::t!(
            "A `block` group is not closed with `end`. Edit it as text."
        ));
    }

    Ok(scanner.document)
}

/// Reads one non-comment line of the body, trying the statements in the order
/// `merman` tries them.
fn scan_statement(scanner: &mut Scanner, line: &str) -> Result<()> {
    // `merman` looks for a link after a block across the line break, so a
    // line opening with one (`x-->b`, which the model would read as a block
    // `x`) continues the statement above instead.
    if std::mem::take(&mut scanner.link_may_follow) && parse_link(&mut Cursor::new(line)).is_some()
    {
        bail!(i18n::t!(
            "Mermaid reads `{line}` as an edge that continues from the block on the previous line. Write an edge on the same line as the blocks at both of its ends."
        ));
    }

    if let Some(tail) = word_tail(line, "end") {
        if scanner.depth() == 0 {
            bail!(i18n::t!(
                "There is an `end` without a matching `block`. Mermaid draws it as a block named `end`."
            ));
        }
        if !tail.is_empty() {
            bail!(i18n::t!(
                "The structured editor does not support `{tail}` written right after `end`. Write it on the next line."
            ));
        }
        scanner.open_groups.pop();
        return Ok(());
    }

    if let Some(header) = line.strip_prefix("block:") {
        let group = parse_group_header(header, line)?;
        // The header is read as a block statement, whose link is looked for
        // before its width.
        scanner.link_may_follow = group.width.is_none();
        let index = scanner.document.entries.len();
        scanner.push_entry(false, BlockItem::Group(group), line)?;
        scanner.open_groups.push(index);
        return Ok(());
    }

    for keyword in ["block-beta", "block"] {
        if let Some(tail) = word_tail(line, keyword) {
            let index = scanner.document.entries.len();
            scanner.push_entry(
                false,
                BlockItem::Group(BlockGroup {
                    id: None,
                    label: None,
                    width: None,
                    columns: None,
                }),
                line,
            )?;
            scanner.open_groups.push(index);
            if !tail.is_empty() {
                scanner.read_items(tail, line)?;
            }
            return Ok(());
        }
    }

    if let Some(tail) = word_tail(line, "columns") {
        let Some(columns) = BlockColumns::from_text(tail) else {
            bail!(i18n::t!(
                "`{line}` cannot be read as a number of columns (a whole number of 1 or more, or auto)."
            ));
        };
        let slot = scanner.columns_mut();
        if slot.is_some() {
            bail!(i18n::t!(
                "There are two `columns` at the same level. Mermaid uses only the last one, so merge them into one."
            ));
        }
        *slot = Some(columns);
        return Ok(());
    }

    if let Some(tail) = word_tail(line, "classDef") {
        let (name, css) = split_first_word(tail);
        if !is_valid_block_class_name(name) {
            bail!(i18n::t!(
                "The structured editor does not support the class name in `{line}`."
            ));
        }
        scanner.document.statements.push(BlockStatement::ClassDef {
            name: name.to_string(),
            css: css.to_string(),
        });
        return Ok(());
    }

    if let Some(tail) = word_tail(line, "class") {
        let (list, class_name) = split_first_word(tail);
        let targets = list
            .split(',')
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if targets.is_empty() || !is_valid_block_class_name(class_name) {
            bail!(i18n::t!(
                "The structured editor does not support `{line}` (write it as `class ID1,ID2 className`)."
            ));
        }
        for target in &targets {
            scanner.require_declared(target, line)?;
        }
        scanner.document.statements.push(BlockStatement::Class {
            targets,
            class_name: class_name.to_string(),
        });
        return Ok(());
    }

    if let Some(tail) = word_tail(line, "style") {
        let (target, css) = split_first_word(tail);
        if target.is_empty() || target.contains(',') {
            bail!(i18n::t!(
                "The structured editor does not support `{line}` (write it as `style ID style`)."
            ));
        }
        scanner.require_declared(target, line)?;
        scanner.document.statements.push(BlockStatement::Style {
            target: target.to_string(),
            css: css.to_string(),
        });
        return Ok(());
    }

    scanner.read_items(line, line)
}

/// What follows `keyword` at the start of `line`, the way `merman`'s block
/// parser matches a keyword: case sensitive, and ended by whitespace, `:` or
/// the end of the line. `None` when the line does not start with it.
fn word_tail<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    match rest.chars().next() {
        None => Some(""),
        // `columns:3` and the like are keywords too, and then fail to parse;
        // the colon is kept so that the reason names it.
        Some(character) if character.is_whitespace() || character == ':' => Some(rest.trim()),
        Some(_) => None,
    }
}

/// The first whitespace separated word of `text` and the trimmed rest.
fn split_first_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(char::is_whitespace) {
        Some(end) => (
            text.get(..end).unwrap_or_default(),
            text.get(end..).unwrap_or_default().trim(),
        ),
        None => (text, ""),
    }
}

fn parse_group_header(header: &str, line: &str) -> Result<BlockGroup> {
    let mut cursor = Cursor::new(header);
    let (id, label) = match parse_block_reference(&mut cursor, line)? {
        ParsedBlock::Node { id, shape } => (id, shape.map(|(_, label)| label)),
        ParsedBlock::Arrow { .. } => {
            bail!(i18n::t!(
                "The group in `{line}` cannot have an arrow shape."
            ));
        }
    };
    cursor.skip_blanks();
    let width = if cursor.eat(":") {
        Some(parse_width(&mut cursor, line)?)
    } else {
        None
    };
    cursor.skip_blanks();
    if !cursor.is_done() {
        bail!(i18n::t!(
            "The structured editor does not support `{}` written right after the group heading in `{line}`. Write it on the next line.",
            cursor.rest()
        ));
    }
    Ok(BlockGroup {
        id: Some(id),
        label,
        width,
        columns: None,
    })
}

struct Cursor<'a> {
    text: &'a str,
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, position: 0 }
    }

    fn rest(&self) -> &'a str {
        self.text.get(self.position..).unwrap_or_default()
    }

    fn is_done(&self) -> bool {
        self.rest().is_empty()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        Some(character)
    }

    fn skip_blanks(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.rest().starts_with(prefix)
    }

    fn eat(&mut self, prefix: &str) -> bool {
        if self.starts_with(prefix) {
            self.position += prefix.len();
            return true;
        }
        false
    }

    /// Whether `keyword` starts here as a word: followed by whitespace, `:` or
    /// nothing.
    fn at_keyword(&self, keyword: &str) -> bool {
        self.rest().strip_prefix(keyword).is_some_and(|after| {
            after
                .chars()
                .next()
                .is_none_or(|next| next.is_whitespace() || next == ':')
        })
    }

    /// The text between `start` and the cursor.
    fn since(&self, start: usize) -> &'a str {
        self.text.get(start..self.position).unwrap_or_default()
    }
}

/// Reads a line of block statements the way `merman` reads them, and whether
/// the line ends with a block after which `merman` looks for a link on the
/// next line.
fn parse_line_tokens(text: &str, line: &str) -> Result<(Vec<LineToken>, bool)> {
    let mut cursor = Cursor::new(text);
    let mut tokens = Vec::new();
    let mut link_may_follow = false;
    loop {
        cursor.skip_blanks();
        if cursor.is_done() {
            break;
        }
        link_may_follow = false;
        if cursor.starts_with("%%") {
            bail!(i18n::t!(
                "The structured editor does not support a `%%` comment in the middle of a line ({line}). Write the comment on a line of its own."
            ));
        }
        if cursor.at_keyword("space") {
            cursor.eat("space");
            tokens.push(LineToken::Space(parse_space_width(&mut cursor, line)?));
            continue;
        }

        let left = parse_block_reference(&mut cursor, line)?;

        // `a space b` is one statement in `merman`: the block after the space
        // is read without a width or an edge of its own.
        let mut probe = Cursor::new(cursor.rest());
        probe.skip_blanks();
        if probe.at_keyword("space") {
            cursor.skip_blanks();
            cursor.eat("space");
            let width = parse_space_width(&mut cursor, line)?;
            tokens.push(LineToken::Block(left, None));
            tokens.push(LineToken::Space(width));
            cursor.skip_blanks();
            if cursor.is_done() || cursor.starts_with("%%") {
                continue;
            }
            if cursor.at_keyword("space") {
                bail!(i18n::t!(
                    "Mermaid reads the second `space` in `{line}` as a block named `space`. The structured editor does not support it."
                ));
            }
            let right = parse_block_reference(&mut cursor, line)?;
            tokens.push(LineToken::Block(right, None));
            cursor.skip_blanks();
            if cursor.starts_with(":") {
                bail!(i18n::t!(
                    "The block right after `space` in `{line}` cannot have a width (Mermaid cannot read it)."
                ));
            }
            continue;
        }

        cursor.skip_blanks();
        if let Some((label, link)) = parse_link(&mut cursor) {
            let right = parse_block_reference(&mut cursor, line)?;
            tokens.push(LineToken::Edge {
                from: left,
                to: right,
                label,
                link,
            });
            cursor.skip_blanks();
            if cursor.starts_with(":") {
                bail!(i18n::t!(
                    "Mermaid cannot read the `:` after the edge in `{line}`."
                ));
            }
            continue;
        }

        cursor.skip_blanks();
        let width = if cursor.eat(":") {
            Some(parse_width(&mut cursor, line)?)
        } else {
            None
        };
        link_may_follow = width.is_none();
        tokens.push(LineToken::Block(left, width));
    }
    Ok((tokens, link_may_follow))
}

/// The width after `space`, `space:3`, which `merman` also reads with blanks
/// around the colon.
fn parse_space_width(cursor: &mut Cursor, line: &str) -> Result<Option<u32>> {
    let mut probe = Cursor::new(cursor.rest());
    probe.skip_blanks();
    if !probe.starts_with(":") {
        return Ok(None);
    }
    cursor.skip_blanks();
    cursor.eat(":");
    parse_width(cursor, line).map(Some)
}

fn parse_width(cursor: &mut Cursor, line: &str) -> Result<u32> {
    cursor.skip_blanks();
    let start = cursor.position;
    while cursor
        .peek()
        .is_some_and(|character| character.is_ascii_digit())
    {
        cursor.bump();
    }
    let Some(width) = parse_positive(cursor.since(start)) else {
        bail!(i18n::t!(
            "The width in `{line}` must be a whole number of 1 or more."
        ));
    };
    Ok(width)
}

/// The bracket pairs `merman` tries after an id, in its order, with the ends
/// it accepts for each.
const NODE_DELIMITERS: &[(&str, &[&str])] = &[
    ("([", &["])"]),
    ("[[", &["]]"]),
    ("[(", &[")]"]),
    ("(((", &[")))"]),
    ("((", &["))", ")"]),
    ("{{", &["}}"]),
    ("[/", &["/]", "\\]", "]"]),
    ("[\\", &["\\]", "/]", "]"]),
    ("[", &["\\]", "/]", "]"]),
    ("(", &[")"]),
    ("{", &["}"]),
    (">", &["]"]),
];

/// Reads a block as an edge end or a statement: an id, optionally followed by
/// a shaped label or an arrow.
fn parse_block_reference(cursor: &mut Cursor, line: &str) -> Result<ParsedBlock> {
    cursor.skip_blanks();
    let start = cursor.position;
    while cursor.peek().is_some_and(|character| {
        !character.is_whitespace()
            && !matches!(
                character,
                '(' | '[' | '-' | ')' | '{' | '}' | '<' | '>' | ':'
            )
    }) {
        cursor.bump();
    }
    let id = cursor.since(start).to_string();
    if id.is_empty() {
        bail!(i18n::t!(
            "The structured editor does not support `{line}` (it could not be read as a block)."
        ));
    }
    if !is_valid_block_id(&id) {
        bail!(i18n::t!(
            "The structured editor does not support `{id}` in `{line}`. {}",
            invalid_id_message(&id)
        ));
    }
    cursor.skip_blanks();

    if cursor.eat("<[") {
        cursor.skip_blanks();
        let label = parse_plain_string(cursor, line)?;
        cursor.skip_blanks();
        if !cursor.eat("]>") {
            bail!(i18n::t!("Close the arrow block in `{line}` with `]>`."));
        }
        cursor.skip_blanks();
        if !cursor.eat("(") {
            bail!(i18n::t!(
                "Write a direction such as `(right)` for the arrow block in `{line}`."
            ));
        }
        let mut directions = Vec::new();
        loop {
            cursor.skip_blanks();
            let start = cursor.position;
            while cursor.peek().is_some_and(|character| {
                !character.is_whitespace() && !matches!(character, ',' | ')')
            }) {
                cursor.bump();
            }
            let word = cursor.since(start);
            let Some(direction) = BlockArrowDirection::from_keyword(word) else {
                bail!(i18n::t!(
                    "The arrow direction `{word}` in `{line}` cannot be read (right / left / up / down / x / y)."
                ));
            };
            directions.push(direction);
            cursor.skip_blanks();
            if !cursor.eat(",") {
                break;
            }
        }
        cursor.skip_blanks();
        if !cursor.eat(")") {
            bail!(i18n::t!("Close the arrow direction in `{line}` with `)`."));
        }
        return Ok(ParsedBlock::Arrow {
            id,
            label,
            directions,
        });
    }

    let Some((start_delimiter, ends)) = NODE_DELIMITERS
        .iter()
        .find(|(start, _)| cursor.starts_with(start))
    else {
        return Ok(ParsedBlock::Node { id, shape: None });
    };
    cursor.eat(start_delimiter);
    cursor.skip_blanks();
    let label = parse_label_string(cursor, line)?;
    cursor.skip_blanks();
    let Some(end_delimiter) = ends.iter().find(|end| cursor.eat(end)) else {
        bail!(i18n::t!(
            "The bracket of block `{id}` in `{line}` is not closed."
        ));
    };
    let Some(shape) = BlockShape::from_delimiters(start_delimiter, end_delimiter) else {
        bail!(i18n::t!(
            "Mermaid does not read the pair `{start_delimiter}…{end_delimiter}` in `{line}` as a shape (it is drawn as a plain box). Change it to `[ ]` or another shape, then open the editor again."
        ));
    };
    Ok(ParsedBlock::Node {
        id,
        shape: Some((shape, label)),
    })
}

/// A quoted label, or a Markdown one (`"`…`"`), as the text between the
/// quotes.
fn parse_label_string(cursor: &mut Cursor, line: &str) -> Result<String> {
    if cursor.starts_with("\"`") {
        let inner = cursor.rest().get(2..).unwrap_or_default();
        let Some(end) = inner.find("`\"") else {
            bail!(i18n::t!(
                "The Markdown label in `{line}` is not closed (a ` is needed before the `\"`)."
            ));
        };
        let label = format!("`{}`", inner.get(..end).unwrap_or_default());
        cursor.position += 2 + end + 2;
        return Ok(label);
    }
    parse_plain_string(cursor, line)
}

fn parse_plain_string(cursor: &mut Cursor, line: &str) -> Result<String> {
    if !cursor.eat("\"") {
        bail!(i18n::t!(
            "Enclose the label in `{line}` in `\"` (labels in a block diagram need quotes)."
        ));
    }
    let rest = cursor.rest();
    let Some(end) = rest.find('"') else {
        bail!(i18n::t!("The `\"` of the label in `{line}` is not closed."));
    };
    let label = rest.get(..end).unwrap_or_default().to_string();
    cursor.position += end + 1;
    Ok(label)
}

/// Reads a link after a block, `-->` or `-- "label" -->`, the way `merman`
/// does, or leaves the cursor where it was when there is none.
fn parse_link(cursor: &mut Cursor) -> Option<(Option<String>, String)> {
    let snapshot = cursor.position;
    if cursor
        .peek()
        .is_some_and(|character| matches!(character, 'x' | 'o' | '<'))
    {
        cursor.bump();
    }
    if cursor.eat("--") || cursor.eat("==") || cursor.eat("-.") {
        cursor.skip_blanks();
        if cursor.eat("\"") {
            let rest = cursor.rest();
            if let Some(end) = rest.find('"') {
                let label = rest.get(..end).unwrap_or_default().to_string();
                cursor.position += end + 1;
                cursor.skip_blanks();
                if let Some(link) = read_link_marker(cursor) {
                    return Some((Some(label), link));
                }
            }
        }
    }
    cursor.position = snapshot;
    read_link_marker(cursor).map(|link| (None, link))
}

fn read_link_marker(cursor: &mut Cursor) -> Option<String> {
    let start = cursor.position;
    while cursor
        .peek()
        .is_some_and(|character| matches!(character, '-' | '=' | '.' | 'x' | 'o' | '<' | '>' | '~'))
    {
        cursor.bump();
    }
    let token = cursor.since(start);
    if is_valid_block_link(token) {
        return Some(token.to_string());
    }
    cursor.position = start;
    None
}

/// What `merman` would take the next block statement on the same line for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineState {
    /// Nothing, or a statement that ends where it ends.
    Fresh,
    /// A block, after which a `space` is read as part of its statement.
    AfterBlock,
    /// `a space`, after which the next block is read without a width, and a
    /// second `space` as a block named `space`.
    AfterInfixSpace,
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_block(document: &BlockDocument) -> String {
    let mut out = String::new();
    for line in &document.preserved_lines {
        if line.position == PreservedPosition::Preamble {
            out.push_str(&line.text);
            out.push('\n');
        }
    }
    out.push_str(document.keyword.keyword());
    out.push('\n');

    let mut writer = LineWriter::default();
    // The `columns` of each open container, until they are written.
    let mut pending_columns: Vec<Option<BlockColumns>> = vec![document.columns];
    let mut previous: Option<(usize, bool)> = None;
    for entry in &document.entries {
        // An outline the form got wrong is written as the closest outline it
        // can be, so that nothing is lost; `structure_error` reports it.
        let depth = entry.depth.min(allowed_depth(previous));
        previous = Some((depth, matches!(entry.item, BlockItem::Group(_))));

        while pending_columns.len() > depth + 1 {
            close_container(&mut out, &mut writer, &mut pending_columns);
        }

        if let BlockItem::Comment(text) = &entry.item {
            writer.flush(&mut out);
            push_line(&mut out, depth, text);
            continue;
        }
        if let Some(columns) = pending_columns.last_mut().and_then(Option::take) {
            writer.flush(&mut out);
            push_line(&mut out, depth, &format!("columns {}", columns.text()));
        }

        match &entry.item {
            BlockItem::Group(group) => {
                writer.flush(&mut out);
                push_line(&mut out, depth, &group_header(group));
                pending_columns.push(group.columns);
            }
            item => writer.push(&mut out, depth, entry.joined, item),
        }
    }
    while pending_columns.len() > 1 {
        close_container(&mut out, &mut writer, &mut pending_columns);
    }
    writer.flush(&mut out);
    if let Some(columns) = pending_columns.last_mut().and_then(Option::take) {
        push_line(&mut out, 0, &format!("columns {}", columns.text()));
    }

    for statement in &document.statements {
        push_line(&mut out, 0, &statement_text(statement));
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn close_container(
    out: &mut String,
    writer: &mut LineWriter,
    pending_columns: &mut Vec<Option<BlockColumns>>,
) {
    writer.flush(out);
    let depth = pending_columns.len().saturating_sub(1);
    if let Some(columns) = pending_columns.pop().flatten() {
        push_line(out, depth, &format!("columns {}", columns.text()));
    }
    push_line(out, depth.saturating_sub(1), "end");
}

/// Writes `text` indented for `depth` (the body of the diagram is depth 0).
fn push_line(out: &mut String, depth: usize, text: &str) {
    for _ in 0..=depth {
        out.push_str(INDENT);
    }
    out.push_str(text);
    out.push('\n');
}

#[derive(Default)]
struct LineWriter {
    tokens: Vec<String>,
    depth: usize,
    state: Option<LineState>,
}

impl LineWriter {
    fn flush(&mut self, out: &mut String) {
        if !self.tokens.is_empty() {
            push_line(out, self.depth, &self.tokens.join(" "));
        }
        self.tokens.clear();
        self.state = None;
    }

    fn push(&mut self, out: &mut String, depth: usize, joined: bool, item: &BlockItem) {
        let is_space = matches!(item, BlockItem::Space { .. });
        let has_width = match item {
            BlockItem::Node(node) => node.width.is_some(),
            BlockItem::Arrow(arrow) => arrow.width.is_some(),
            _ => false,
        };
        let fits = match self.state {
            Some(LineState::AfterInfixSpace) => !is_space && !has_width,
            Some(_) => true,
            None => false,
        };
        if !(joined && fits && self.depth == depth) {
            self.flush(out);
            self.depth = depth;
        }
        let state = self.state.unwrap_or(LineState::Fresh);
        // A block with a width has its statement ended by the width, so a
        // `space` after it stands on its own.
        self.state = Some(match (state, is_space) {
            (LineState::AfterBlock, true) => LineState::AfterInfixSpace,
            (_, true) | (LineState::AfterInfixSpace, false) => LineState::Fresh,
            (_, false) if has_width => LineState::Fresh,
            (_, false) => LineState::AfterBlock,
        });
        self.tokens.push(item_text(item));
    }
}

fn width_suffix(width: Option<u32>) -> String {
    width.map(|width| format!(":{width}")).unwrap_or_default()
}

fn item_text(item: &BlockItem) -> String {
    match item {
        BlockItem::Node(node) => {
            let shape = node
                .shape
                .map(|shape| {
                    let (start, end) = shape.delimiters();
                    format!("{start}\"{}\"{end}", node.label)
                })
                .unwrap_or_default();
            format!("{}{shape}{}", node.id, width_suffix(node.width))
        }
        BlockItem::Arrow(arrow) => format!(
            "{}<[\"{}\"]>({}){}",
            arrow.id,
            arrow.label,
            arrow_directions_text(&arrow.directions),
            width_suffix(arrow.width)
        ),
        BlockItem::Space { width } => format!("space{}", width_suffix(*width)),
        BlockItem::Group(group) => group_header(group),
        BlockItem::Comment(text) => text.clone(),
    }
}

fn group_header(group: &BlockGroup) -> String {
    let Some(id) = &group.id else {
        return "block".to_string();
    };
    let label = group
        .label
        .as_ref()
        .map(|label| format!("[\"{label}\"]"))
        .unwrap_or_default();
    format!("block:{id}{label}{}", width_suffix(group.width))
}

fn statement_text(statement: &BlockStatement) -> String {
    let with_rest = |head: String, rest: &str| {
        if rest.is_empty() {
            head
        } else {
            format!("{head} {rest}")
        }
    };
    match statement {
        BlockStatement::Edge(edge) => match &edge.label {
            Some(label) => format!("{} -- \"{label}\" {} {}", edge.from, edge.link, edge.to),
            None => format!("{} {} {}", edge.from, edge.link, edge.to),
        },
        BlockStatement::ClassDef { name, css } => with_rest(format!("classDef {name}"), css),
        BlockStatement::Class {
            targets,
            class_name,
        } => format!("class {} {class_name}", targets.join(",")),
        BlockStatement::Style { target, css } => with_rest(format!("style {target}"), css),
        BlockStatement::Comment(text) => text.clone(),
    }
}

#[cfg(test)]
#[path = "block_model_tests.rs"]
mod tests;
