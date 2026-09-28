//! A round-trippable editing model for Mermaid C4 diagrams.
//!
//! [`parse_c4`] reads the body of a ```` ```mermaid ```` fence into
//! [`C4Document`], and [`emit_c4`] writes it back. See [`crate::mermaid_model`]
//! for the invariants every diagram kind keeps.
//!
//! `merman` reads a C4 diagram one line at a time, and the order of the lines
//! is part of what they mean: a boundary holds what is written between its
//! `{` and `}`, `UpdateElementStyle` only reaches an element declared above it,
//! and a second `Rel` between the same two elements replaces the first. The
//! document is therefore one flat list in source order, with a boundary written
//! as a start and an end item, rather than a tree that would have to decide
//! where the lines between its branches go.
//!
//! Every macro keeps its arguments as written — positional or `$name="…"` —
//! because what a position means depends on the macro (`Person` has its
//! description third, `Container` its technology). The form edits the few
//! positions that are the same kind of text everywhere; the rest is written
//! back as it came.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the spacing inside a macro call, a `{` on the line after its
//! boundary (joined onto the boundary's line), an empty argument (written as
//! `""`), and a `%%` comment after a statement (moved onto its own line below
//! the statement).

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::PreservedLine;
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADERS: &[&str] = &[
    "C4Context",
    "C4Container",
    "C4Component",
    "C4Dynamic",
    "C4Deployment",
];

/// Which of the five C4 diagrams the header names. `merman` draws every macro
/// in every one of them; the kind decides the heading and which element
/// macros the form offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C4DiagramType {
    Context,
    Container,
    Component,
    Dynamic,
    Deployment,
}

impl C4DiagramType {
    pub const ALL: [Self; 5] = [
        Self::Context,
        Self::Container,
        Self::Component,
        Self::Dynamic,
        Self::Deployment,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Context => "C4Context",
            Self::Container => "C4Container",
            Self::Component => "C4Component",
            Self::Dynamic => "C4Dynamic",
            Self::Deployment => "C4Deployment",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Context => "コンテキスト図",
            Self::Container => "コンテナー図",
            Self::Component => "コンポーネント図",
            Self::Dynamic => "動的図",
            Self::Deployment => "配置図",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|diagram_type| diagram_type.keyword() == keyword)
    }

    /// The element macros the form lists for this kind of diagram.
    pub fn element_macros(self) -> Vec<&'static str> {
        C4_ELEMENT_MACROS
            .iter()
            .copied()
            .filter(|name| match self {
                Self::Context => element_family(name) == Some(C4ElementFamily::PersonOrSystem),
                Self::Container => !name.starts_with("Component"),
                Self::Component | Self::Dynamic | Self::Deployment => true,
            })
            .collect()
    }

    /// The macro a new element is added with.
    pub fn default_element_macro(self) -> &'static str {
        match self {
            Self::Context => "System",
            Self::Container | Self::Deployment => "Container",
            Self::Component | Self::Dynamic => "Component",
        }
    }

    /// The macro a new boundary is added with.
    pub fn default_boundary_macro(self) -> &'static str {
        match self {
            Self::Context | Self::Dynamic => "Boundary",
            Self::Container => "System_Boundary",
            Self::Component => "Container_Boundary",
            Self::Deployment => "Deployment_Node",
        }
    }
}

/// The element macros `merman` reads, in the order the form lists them.
pub const C4_ELEMENT_MACROS: [&str; 20] = [
    "Person",
    "Person_Ext",
    "System",
    "SystemDb",
    "SystemQueue",
    "System_Ext",
    "SystemDb_Ext",
    "SystemQueue_Ext",
    "Container",
    "ContainerDb",
    "ContainerQueue",
    "Container_Ext",
    "ContainerDb_Ext",
    "ContainerQueue_Ext",
    "Component",
    "ComponentDb",
    "ComponentQueue",
    "Component_Ext",
    "ComponentDb_Ext",
    "ComponentQueue_Ext",
];

/// The boundary macros `merman` reads. Each opens a `{ … }` block.
pub const C4_BOUNDARY_MACROS: [&str; 8] = [
    "Boundary",
    "Enterprise_Boundary",
    "System_Boundary",
    "Container_Boundary",
    "Node",
    "Deployment_Node",
    "Node_L",
    "Node_R",
];

/// The relationship macros `merman` reads. `Rel_Up` and its like are other
/// spellings of `Rel_U` and theirs, and `RelIndex` takes an index first that
/// `merman` ignores.
pub const C4_RELATION_MACROS: [&str; 12] = [
    "Rel",
    "BiRel",
    "Rel_U",
    "Rel_D",
    "Rel_L",
    "Rel_R",
    "Rel_Back",
    "Rel_Up",
    "Rel_Down",
    "Rel_Left",
    "Rel_Right",
    "RelIndex",
];

/// The relationship macros the form offers; the other spellings are kept when
/// a diagram already uses them.
pub const C4_OFFERED_RELATION_MACROS: [&str; 7] = [
    "Rel", "BiRel", "Rel_U", "Rel_D", "Rel_L", "Rel_R", "Rel_Back",
];

const STYLE_MACROS: [&str; 3] = ["UpdateElementStyle", "UpdateRelStyle", "UpdateLayoutConfig"];

/// A short Japanese name for a macro, shown beside the macro in the menus.
pub fn c4_macro_label(name: &str) -> &'static str {
    match name {
        "Person" => "人",
        "Person_Ext" => "外部の人",
        "System" => "システム",
        "SystemDb" => "システム (DB)",
        "SystemQueue" => "システム (キュー)",
        "System_Ext" => "外部システム",
        "SystemDb_Ext" => "外部システム (DB)",
        "SystemQueue_Ext" => "外部システム (キュー)",
        "Container" => "コンテナー",
        "ContainerDb" => "コンテナー (DB)",
        "ContainerQueue" => "コンテナー (キュー)",
        "Container_Ext" => "外部コンテナー",
        "ContainerDb_Ext" => "外部コンテナー (DB)",
        "ContainerQueue_Ext" => "外部コンテナー (キュー)",
        "Component" => "コンポーネント",
        "ComponentDb" => "コンポーネント (DB)",
        "ComponentQueue" => "コンポーネント (キュー)",
        "Component_Ext" => "外部コンポーネント",
        "ComponentDb_Ext" => "外部コンポーネント (DB)",
        "ComponentQueue_Ext" => "外部コンポーネント (キュー)",
        "Boundary" => "境界",
        "Enterprise_Boundary" => "企業の境界",
        "System_Boundary" => "システムの境界",
        "Container_Boundary" => "コンテナーの境界",
        "Node" | "Deployment_Node" => "配置ノード",
        "Node_L" => "配置ノード (左寄せ)",
        "Node_R" => "配置ノード (右寄せ)",
        "Rel" | "RelIndex" => "関係",
        "BiRel" => "双方向",
        "Rel_U" | "Rel_Up" => "上向き",
        "Rel_D" | "Rel_Down" => "下向き",
        "Rel_L" | "Rel_Left" => "左向き",
        "Rel_R" | "Rel_Right" => "右向き",
        "Rel_Back" => "逆向き",
        _ => "",
    }
}

/// How an element macro lays out its arguments. `merman` reads a person or a
/// system as `(alias, label, descr, sprite, tags, link)` and a container or a
/// component as `(alias, label, techn, descr, sprite, tags, link)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C4ElementFamily {
    PersonOrSystem,
    ContainerOrComponent,
}

pub fn element_family(name: &str) -> Option<C4ElementFamily> {
    if !C4_ELEMENT_MACROS.contains(&name) {
        return None;
    }
    if name.starts_with("Person") || name.starts_with("System") {
        Some(C4ElementFamily::PersonOrSystem)
    } else {
        Some(C4ElementFamily::ContainerOrComponent)
    }
}

/// How a boundary macro lays out its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C4BoundaryFamily {
    /// `Boundary(alias, label, type, tags, link)`.
    Generic,
    /// `Enterprise_Boundary`, `System_Boundary` and `Container_Boundary`:
    /// `merman` puts the type in itself, so they take `(alias, label, tags,
    /// link)`.
    Typed,
    /// `Node` and its like: `(alias, label, type, descr, sprite, tags, link)`,
    /// with the sprite ignored.
    Node,
}

pub fn boundary_family(name: &str) -> Option<C4BoundaryFamily> {
    match name {
        "Boundary" => Some(C4BoundaryFamily::Generic),
        "Enterprise_Boundary" | "System_Boundary" | "Container_Boundary" => {
            Some(C4BoundaryFamily::Typed)
        }
        "Node" | "Deployment_Node" | "Node_L" | "Node_R" => Some(C4BoundaryFamily::Node),
        _ => None,
    }
}

/// One argument of a macro call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C4Argument {
    /// A positional argument. `quoted` records how it was written, so that an
    /// alias written bare stays bare; an empty one is always quoted, since a
    /// bare empty argument at the end of the list is not an argument at all.
    Text { value: String, quoted: bool },
    /// `$key="value"`.
    Named { key: String, value: String },
}

impl C4Argument {
    fn text(value: &str) -> Self {
        Self::Text {
            value: value.to_string(),
            quoted: true,
        }
    }

    fn bare(value: &str) -> Self {
        Self::Text {
            value: value.to_string(),
            quoted: false,
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { value, .. } => Some(value),
            Self::Named { .. } => None,
        }
    }
}

/// One of the texts the form edits on an element, a boundary or a
/// relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C4Field {
    Label,
    Technology,
    Description,
    /// The kind written in brackets on a `Boundary` or a deployment node.
    Type,
}

impl C4Field {
    pub const ALL: [Self; 4] = [Self::Label, Self::Technology, Self::Description, Self::Type];

    /// The name a `$name="…"` argument sets the same text with.
    fn key(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Technology => "techn",
            Self::Description => "descr",
            Self::Type => "type",
        }
    }
}

/// What a field shows for one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C4FieldValue {
    /// The macro has no such text.
    Unavailable,
    /// The text is set by a `$name="…"` argument, whose effect depends on
    /// where it is written; the form leaves it as it is.
    Locked,
    /// The text as drawn, which is the default when the position is not
    /// written.
    Value(String),
}

/// A macro call: an element, a boundary, a relationship or a style update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C4Statement {
    pub name: String,
    pub arguments: Vec<C4Argument>,
}

impl C4Statement {
    pub fn new(name: &str, arguments: Vec<C4Argument>) -> Self {
        Self {
            name: name.to_string(),
            arguments,
        }
    }

    /// A new element or boundary with its alias and label.
    pub fn declaration(name: &str, alias: &str, label: &str) -> Self {
        Self::new(name, vec![C4Argument::bare(alias), C4Argument::text(label)])
    }

    /// A new relationship from `from` to `to`.
    pub fn relation(name: &str, from: &str, to: &str, label: &str) -> Self {
        Self::new(
            name,
            vec![
                C4Argument::bare(from),
                C4Argument::bare(to),
                C4Argument::text(label),
            ],
        )
    }

    /// Where the relationship's arguments start: `RelIndex` has an index in
    /// front of them.
    fn relation_offset(&self) -> usize {
        usize::from(self.name == "RelIndex")
    }

    /// The alias an element or a boundary declares.
    pub fn alias(&self) -> Option<&str> {
        self.arguments.first().and_then(C4Argument::as_text)
    }

    pub fn set_alias(&mut self, alias: &str) {
        set_text_keeping_style(&mut self.arguments, 0, alias);
    }

    /// The aliases a relationship joins.
    pub fn relation_ends(&self) -> Option<(&str, &str)> {
        let offset = self.relation_offset();
        Some((
            self.arguments.get(offset)?.as_text()?,
            self.arguments.get(offset + 1)?.as_text()?,
        ))
    }

    pub fn set_relation_end(&mut self, is_from: bool, alias: &str) {
        let slot = self.relation_offset() + usize::from(!is_from);
        set_text_keeping_style(&mut self.arguments, slot, alias);
    }

    /// Where `field` is written for this macro, or `None` when it has no such
    /// text.
    pub fn field_slot(&self, field: C4Field) -> Option<usize> {
        if C4_RELATION_MACROS.contains(&self.name.as_str()) {
            let slot = match field {
                C4Field::Label => 2,
                C4Field::Technology => 3,
                C4Field::Description => 4,
                C4Field::Type => return None,
            };
            return Some(slot + self.relation_offset());
        }
        if let Some(family) = element_family(&self.name) {
            return match (family, field) {
                (_, C4Field::Label) => Some(1),
                (C4ElementFamily::PersonOrSystem, C4Field::Description) => Some(2),
                (C4ElementFamily::ContainerOrComponent, C4Field::Technology) => Some(2),
                (C4ElementFamily::ContainerOrComponent, C4Field::Description) => Some(3),
                _ => None,
            };
        }
        match (boundary_family(&self.name)?, field) {
            (_, C4Field::Label) => Some(1),
            (C4BoundaryFamily::Generic | C4BoundaryFamily::Node, C4Field::Type) => Some(2),
            (C4BoundaryFamily::Node, C4Field::Description) => Some(3),
            _ => None,
        }
    }

    /// What `merman` draws for a position nobody wrote.
    fn slot_default(&self, slot: usize) -> &'static str {
        if self.field_slot(C4Field::Type) != Some(slot) {
            return "";
        }
        if boundary_family(&self.name) == Some(C4BoundaryFamily::Node) {
            "node"
        } else {
            "system"
        }
    }

    pub fn field(&self, field: C4Field) -> C4FieldValue {
        let Some(slot) = self.field_slot(field) else {
            return C4FieldValue::Unavailable;
        };
        // A `$descr="…"` anywhere in the call sets the text by name, and which
        // of it and the position wins depends on the order `merman` applies
        // them in, so the form does not offer to edit either.
        let named_elsewhere = self.arguments.iter().any(
            |argument| matches!(argument, C4Argument::Named { key, .. } if key == field.key()),
        );
        if named_elsewhere {
            return C4FieldValue::Locked;
        }
        match self.arguments.get(slot) {
            Some(C4Argument::Text { value, .. }) => C4FieldValue::Value(value.clone()),
            Some(C4Argument::Named { .. }) => C4FieldValue::Locked,
            None => C4FieldValue::Value(self.slot_default(slot).to_string()),
        }
    }

    /// Writes `value` at `field`'s position. A position past the end is
    /// reached by writing the defaults in between, which `merman` reads the
    /// same as leaving them out; a locked or missing field is left alone.
    pub fn set_field(&mut self, field: C4Field, value: &str) {
        let (C4FieldValue::Value(current), Some(slot)) =
            (self.field(field), self.field_slot(field))
        else {
            return;
        };
        if current == value {
            return;
        }
        while self.arguments.len() < slot {
            let default = self.slot_default(self.arguments.len());
            self.arguments.push(C4Argument::text(default));
        }
        match self.arguments.get_mut(slot) {
            Some(argument) => *argument = C4Argument::text(value),
            None => self.arguments.push(C4Argument::text(value)),
        }
    }

    /// Whether the statement can become `name` without losing a text. Only a
    /// macro of the same sort is offered, and a container's technology has no
    /// place on a person or a system.
    pub fn can_become(&self, name: &str) -> bool {
        if C4_RELATION_MACROS.contains(&self.name.as_str()) {
            return C4_RELATION_MACROS.contains(&name) && name != "RelIndex";
        }
        if let Some(family) = boundary_family(&self.name) {
            return boundary_family(name) == Some(family);
        }
        let (Some(from), Some(to)) = (element_family(&self.name), element_family(name)) else {
            return false;
        };
        if from == to || to == C4ElementFamily::ContainerOrComponent {
            return true;
        }
        match self.arguments.get(2) {
            None => true,
            Some(C4Argument::Text { value, .. }) => value.is_empty(),
            Some(C4Argument::Named { .. }) => false,
        }
    }

    /// Turns the statement into `name`, moving the arguments to where the new
    /// macro reads the same texts. Does nothing when [`Self::can_become`]
    /// says no.
    pub fn set_macro(&mut self, name: &str) {
        if self.name == name || !self.can_become(name) {
            return;
        }
        if self.name == "RelIndex" && !self.arguments.is_empty() {
            self.arguments.remove(0);
        }
        match (element_family(&self.name), element_family(name)) {
            (
                Some(C4ElementFamily::PersonOrSystem),
                Some(C4ElementFamily::ContainerOrComponent),
            ) if self.arguments.len() > 2 => {
                self.arguments.insert(2, C4Argument::text(""));
            }
            (
                Some(C4ElementFamily::ContainerOrComponent),
                Some(C4ElementFamily::PersonOrSystem),
            ) if self.arguments.len() > 2 => {
                self.arguments.remove(2);
            }
            _ => {}
        }
        self.name = name.to_string();
    }
}

/// Replaces the text at `slot`, keeping a bare argument bare when the new text
/// can be written bare.
fn set_text_keeping_style(arguments: &mut [C4Argument], slot: usize, value: &str) {
    if let Some(C4Argument::Text {
        value: current,
        quoted,
    }) = arguments.get_mut(slot)
    {
        if !is_valid_c4_alias(value) {
            *quoted = true;
        }
        *current = value.to_string();
    }
}

/// One line of the body — or, for a comment block, a few lines — in source
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C4Item {
    /// `title …`, holding the text after the keyword.
    Title(String),
    Element(C4Statement),
    /// A boundary macro and its `{`. What follows up to the matching
    /// [`C4Item::BoundaryEnd`] is inside it.
    BoundaryStart(C4Statement),
    BoundaryEnd,
    Relation(C4Statement),
    /// `UpdateElementStyle`, `UpdateRelStyle` or `UpdateLayoutConfig`.
    Style(C4Statement),
    /// A comment, an accessibility statement, or a relationship without a
    /// label (which `merman` reads and then ignores), written back as it came.
    Preserved(String),
}

impl C4Item {
    /// Whether the item is drawn in the element list: an element or either
    /// end of a boundary.
    pub fn is_tree_item(&self) -> bool {
        matches!(
            self,
            Self::Element(_) | Self::BoundaryStart(_) | Self::BoundaryEnd
        )
    }

    fn statement(&self) -> Option<&C4Statement> {
        match self {
            Self::Element(statement)
            | Self::BoundaryStart(statement)
            | Self::Relation(statement)
            | Self::Style(statement) => Some(statement),
            _ => None,
        }
    }
}

/// An editable C4 diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct C4Document {
    pub diagram_type: C4DiagramType,
    /// Frontmatter, directives and comments before the header.
    pub preamble: Vec<PreservedLine>,
    pub items: Vec<C4Item>,
}

impl C4Document {
    /// A blank C4 diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            diagram_type: C4DiagramType::Context,
            preamble: Vec::new(),
            items: Vec::new(),
        }
    }

    /// The title. The parser refuses a diagram with two, so there is at most
    /// one.
    pub fn title(&self) -> Option<&str> {
        self.items.iter().find_map(|item| match item {
            C4Item::Title(title) => Some(title.as_str()),
            _ => None,
        })
    }

    /// Replaces the title, adds one, or removes it for `None`. A new title
    /// goes below the `accTitle` lines, which set the title too and would
    /// otherwise replace it.
    pub fn set_title(&mut self, title: Option<&str>) {
        let index = self
            .items
            .iter()
            .position(|item| matches!(item, C4Item::Title(_)));
        match (index, title) {
            (Some(index), Some(title)) => {
                if let Some(item) = self.items.get_mut(index) {
                    *item = C4Item::Title(title.to_string());
                }
            }
            (Some(index), None) => {
                self.items.remove(index);
            }
            (None, Some(title)) => {
                let index = self
                    .items
                    .iter()
                    .rposition(|item| matches!(item, C4Item::Preserved(text) if is_accessible_title(text.trim())))
                    .map_or(0, |index| index + 1);
                self.items.insert(index, C4Item::Title(title.to_string()));
            }
            (None, None) => {}
        }
    }

    /// Whether an `accTitle` line comes after the `title` line — or stands
    /// alone — and so is what `merman` draws as the title.
    pub fn title_comes_from_accessible_title(&self) -> bool {
        let title = self
            .items
            .iter()
            .position(|item| matches!(item, C4Item::Title(_)));
        self.items.iter().enumerate().any(|(index, item)| {
            matches!(item, C4Item::Preserved(text) if is_accessible_title(text.trim()))
                && title.is_none_or(|title| index > title)
        })
    }

    /// How deep in boundaries the item at `index` is. A boundary's `}` is as
    /// deep as its start.
    pub fn depth(&self, index: usize) -> usize {
        let mut depth = 0usize;
        for item in self.items.iter().take(index) {
            match item {
                C4Item::BoundaryStart(_) => depth += 1,
                C4Item::BoundaryEnd => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if matches!(self.items.get(index), Some(C4Item::BoundaryEnd)) {
            depth.saturating_sub(1)
        } else {
            depth
        }
    }

    /// The index of the `}` that closes the boundary starting at `start`.
    pub fn boundary_end(&self, start: usize) -> Option<usize> {
        if !matches!(self.items.get(start), Some(C4Item::BoundaryStart(_))) {
            return None;
        }
        let mut depth = 0usize;
        for (index, item) in self.items.iter().enumerate().skip(start) {
            match item {
                C4Item::BoundaryStart(_) => depth += 1,
                C4Item::BoundaryEnd => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The start of the boundary the item at `index` is in.
    pub fn parent(&self, index: usize) -> Option<usize> {
        let mut open = Vec::new();
        for (position, item) in self.items.iter().enumerate().take(index) {
            match item {
                C4Item::BoundaryStart(_) => open.push(position),
                C4Item::BoundaryEnd => {
                    open.pop();
                }
                _ => {}
            }
        }
        open.last().copied()
    }

    /// The aliases of the elements, in source order.
    pub fn element_aliases(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|item| match item {
                C4Item::Element(statement) => statement.alias().map(str::to_string),
                _ => None,
            })
            .collect()
    }

    /// The starts of the boundaries, in source order, with their aliases.
    pub fn boundaries(&self) -> Vec<(usize, String)> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                C4Item::BoundaryStart(statement) => {
                    Some((index, statement.alias().unwrap_or_default().to_string()))
                }
                _ => None,
            })
            .collect()
    }

    /// An alias no element or boundary has, starting with `prefix`.
    pub fn unused_alias(&self, prefix: &str) -> String {
        let taken = self
            .items
            .iter()
            .filter_map(|item| match item {
                C4Item::Element(statement) | C4Item::BoundaryStart(statement) => statement.alias(),
                _ => None,
            })
            .collect::<Vec<_>>();
        (1..)
            .map(|number| format!("{prefix}{number}"))
            .find(|alias| !taken.contains(&alias.as_str()))
            .unwrap_or_else(|| prefix.to_string())
    }

    /// Where a new element or boundary goes: at the end of the boundary
    /// starting at `parent`, or after the last element or boundary outside
    /// every boundary — above the relationships and style updates, where a
    /// hand-written diagram keeps its declarations.
    pub fn insertion_point(&self, parent: Option<usize>) -> usize {
        if let Some(end) = parent.and_then(|start| self.boundary_end(start)) {
            return end;
        }
        let last_declaration = self
            .items
            .iter()
            .enumerate()
            .filter(|(index, item)| {
                matches!(item, C4Item::Element(_) | C4Item::BoundaryEnd) && self.depth(*index) == 0
            })
            .map(|(index, _)| index)
            .next_back();
        match last_declaration {
            Some(index) => index + 1,
            None => self
                .items
                .iter()
                .position(|item| matches!(item, C4Item::Relation(_) | C4Item::Style(_)))
                .unwrap_or(self.items.len()),
        }
    }

    /// Adds a boundary with nothing in it at `index`.
    pub fn insert_boundary(&mut self, index: usize, statement: C4Statement) {
        let index = index.min(self.items.len());
        self.items.insert(index, C4Item::BoundaryEnd);
        self.items.insert(index, C4Item::BoundaryStart(statement));
    }

    /// Points every relationship and style update that names an alias in
    /// `renames` at its new name. The declarations themselves are renamed by
    /// the caller. The renames apply together, so two aliases can trade
    /// names.
    pub fn rename_references(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let renamed = |alias: &str| {
            renames
                .iter()
                .find(|(old, _)| old == alias)
                .map(|(_, new)| new.clone())
        };
        for item in &mut self.items {
            match item {
                C4Item::Relation(statement) => {
                    let Some((from, to)) = statement
                        .relation_ends()
                        .map(|(from, to)| (renamed(from), renamed(to)))
                    else {
                        continue;
                    };
                    if let Some(from) = from {
                        statement.set_relation_end(true, &from);
                    }
                    if let Some(to) = to {
                        statement.set_relation_end(false, &to);
                    }
                }
                C4Item::Style(statement) => {
                    let slots = match statement.name.as_str() {
                        "UpdateElementStyle" => 1,
                        "UpdateRelStyle" => 2,
                        _ => 0,
                    };
                    for slot in 0..slots {
                        let new = statement
                            .arguments
                            .get(slot)
                            .and_then(C4Argument::as_text)
                            .and_then(renamed);
                        if let Some(new) = new {
                            set_text_keeping_style(&mut statement.arguments, slot, &new);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Removes the element at `index` with the relationships that join it and
    /// the style updates that name it or them — a relationship left pointing
    /// at a missing element stops the whole diagram from being drawn.
    ///
    /// Returns the old index of every item that is left, in its new order.
    pub fn remove_element(&mut self, index: usize) -> Vec<usize> {
        let Some(alias) = (match self.items.get(index) {
            Some(C4Item::Element(statement)) => statement.alias().map(str::to_string),
            _ => None,
        }) else {
            return (0..self.items.len()).collect();
        };
        let removed_pairs = self
            .items
            .iter()
            .filter_map(|item| match item {
                C4Item::Relation(statement) => statement
                    .relation_ends()
                    .filter(|(from, to)| *from == alias || *to == alias)
                    .map(|(from, to)| (from.to_string(), to.to_string())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let keep = self
            .items
            .iter()
            .enumerate()
            .filter(|(position, item)| {
                if *position == index {
                    return false;
                }
                match item {
                    C4Item::Relation(statement) => statement
                        .relation_ends()
                        .is_none_or(|(from, to)| from != alias && to != alias),
                    C4Item::Style(statement) => {
                        !style_names_alias(statement, &alias)
                            && !style_names_pair(statement, &removed_pairs)
                    }
                    _ => true,
                }
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        self.apply_order(&keep);
        keep
    }

    /// Removes the boundary starting at `index` — its `{` and `}` — leaving
    /// what was inside it one level further out, and the style updates that
    /// name it.
    pub fn remove_boundary(&mut self, index: usize) -> Vec<usize> {
        let (Some(end), Some(C4Item::BoundaryStart(statement))) =
            (self.boundary_end(index), self.items.get(index))
        else {
            return (0..self.items.len()).collect();
        };
        let alias = statement.alias().unwrap_or_default().to_string();
        let keep = self
            .items
            .iter()
            .enumerate()
            .filter(|(position, item)| {
                *position != index
                    && *position != end
                    && !matches!(item, C4Item::Style(statement) if style_names_alias(statement, &alias))
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        self.apply_order(&keep);
        keep
    }

    /// Removes the relationship at `index` and the `UpdateRelStyle` that
    /// styles it, unless another relationship between the same pair above the
    /// style is left for it to style.
    pub fn remove_relation(&mut self, index: usize) -> Vec<usize> {
        let Some(pair) = (match self.items.get(index) {
            Some(C4Item::Relation(statement)) => statement
                .relation_ends()
                .map(|(from, to)| (from.to_string(), to.to_string())),
            _ => None,
        }) else {
            return (0..self.items.len()).collect();
        };
        let ends = (pair.0.as_str(), pair.1.as_str());
        let pairs = [pair.clone()];
        let keep = self
            .items
            .iter()
            .enumerate()
            .filter(|(position, item)| {
                if *position == index {
                    return false;
                }
                let C4Item::Style(statement) = item else {
                    return true;
                };
                !style_names_pair(statement, &pairs)
                    || self
                        .items
                        .iter()
                        .take(*position)
                        .enumerate()
                        .any(|(other, earlier)| {
                            other != index
                                && matches!(earlier, C4Item::Relation(relation)
                                    if relation.relation_ends() == Some(ends))
                        })
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        self.apply_order(&keep);
        keep
    }

    /// The range an element or a whole boundary covers.
    fn block(&self, index: usize) -> Option<std::ops::RangeInclusive<usize>> {
        match self.items.get(index)? {
            C4Item::Element(_) => Some(index..=index),
            C4Item::BoundaryStart(_) => Some(index..=self.boundary_end(index)?),
            _ => None,
        }
    }

    /// The element or boundary edge the element or boundary at `index` moves
    /// past, or `None` at the end it is already at.
    fn tree_neighbour(&self, index: usize, forward: bool) -> Option<usize> {
        let block = self.block(index)?;
        if forward {
            self.items
                .iter()
                .enumerate()
                .skip(block.end() + 1)
                .find(|(_, item)| item.is_tree_item())
                .map(|(position, _)| position)
        } else {
            self.items
                .iter()
                .enumerate()
                .take(*block.start())
                .rev()
                .find(|(_, item)| item.is_tree_item())
                .map(|(position, _)| position)
        }
    }

    pub fn can_move_in_tree(&self, index: usize, forward: bool) -> bool {
        self.tree_neighbour(index, forward).is_some()
    }

    /// Moves the element or boundary at `index` one step past the element or
    /// boundary edge next to it. Stepping over a `{` or a `}` is how it moves
    /// into and out of a boundary.
    pub fn move_in_tree(&mut self, index: usize, forward: bool) -> Vec<usize> {
        let identity = (0..self.items.len()).collect::<Vec<_>>();
        let (Some(block), Some(neighbour)) =
            (self.block(index), self.tree_neighbour(index, forward))
        else {
            return identity;
        };
        let moved = block.clone().collect::<Vec<_>>();
        let mut order = identity
            .into_iter()
            .filter(|position| !block.contains(position))
            .collect::<Vec<_>>();
        let Some(anchor) = order.iter().position(|position| *position == neighbour) else {
            return (0..self.items.len()).collect();
        };
        let at = if forward { anchor + 1 } else { anchor };
        order.splice(at..at, moved);
        self.apply_order(&order);
        order
    }

    /// Moves the element at `index` to the end of the boundary starting at
    /// `parent`, or out of every boundary for `None`.
    pub fn move_element_to(&mut self, index: usize, parent: Option<usize>) -> Vec<usize> {
        let identity = (0..self.items.len()).collect::<Vec<_>>();
        if !matches!(self.items.get(index), Some(C4Item::Element(_)))
            || self.parent(index) == parent
            || parent.is_some_and(|start| self.boundary_end(start).is_none())
        {
            return identity;
        }
        let mut order = identity
            .into_iter()
            .filter(|position| *position != index)
            .collect::<Vec<_>>();
        let mut without = self.clone();
        without.items.remove(index);
        let parent = parent.map(|start| if start > index { start - 1 } else { start });
        let at = without.insertion_point(parent).min(order.len());
        order.insert(at, index);
        self.apply_order(&order);
        order
    }

    /// The relationship before or after the one at `index`.
    fn relation_neighbour(&self, index: usize, forward: bool) -> Option<usize> {
        let is_relation = |(_, item): &(usize, &C4Item)| matches!(item, C4Item::Relation(_));
        if forward {
            self.items
                .iter()
                .enumerate()
                .skip(index + 1)
                .find(is_relation)
                .map(|(position, _)| position)
        } else {
            self.items
                .iter()
                .enumerate()
                .take(index)
                .rev()
                .find(is_relation)
                .map(|(position, _)| position)
        }
    }

    pub fn can_move_relation(&self, index: usize, forward: bool) -> bool {
        matches!(self.items.get(index), Some(C4Item::Relation(_)))
            && self.relation_neighbour(index, forward).is_some()
    }

    /// Swaps the relationship at `index` with the one before or after it.
    pub fn move_relation(&mut self, index: usize, forward: bool) -> Vec<usize> {
        let mut order = (0..self.items.len()).collect::<Vec<_>>();
        if !self.can_move_relation(index, forward) {
            return order;
        }
        if let Some(other) = self.relation_neighbour(index, forward) {
            order.swap(index, other);
            self.items.swap(index, other);
        }
        order
    }

    /// The elements (or, for `boundaries`, the boundaries) with the alias of
    /// the boundary each is in, the way `merman` records them: a repeated
    /// alias keeps its first place and takes the last one's boundary.
    fn declarations_as_merman_reads_them(&self, boundaries: bool) -> Vec<(String, String)> {
        let mut declarations: Vec<(String, String)> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let statement = match item {
                C4Item::Element(statement) if !boundaries => statement,
                C4Item::BoundaryStart(statement) if boundaries => statement,
                _ => continue,
            };
            let alias = statement.alias().unwrap_or_default().to_string();
            // `merman` merges a boundary named `global` into the whole
            // diagram, which `structure_error` reports.
            if boundaries && alias == "global" {
                continue;
            }
            let parent = self
                .parent(index)
                .and_then(|start| self.items.get(start))
                .and_then(C4Item::statement)
                .and_then(C4Statement::alias)
                .unwrap_or("global")
                .to_string();
            match declarations.iter_mut().find(|(known, _)| *known == alias) {
                Some(declaration) => declaration.1 = parent,
                None => declarations.push((alias, parent)),
            }
        }
        declarations
    }

    /// The ordered pairs the relationships join, each once, in the order
    /// `merman` first meets them.
    fn relation_pairs_as_merman_reads_them(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for item in &self.items {
            if let C4Item::Relation(statement) = item
                && let Some((from, to)) = statement.relation_ends()
                && !pairs
                    .iter()
                    .any(|(known_from, known_to)| known_from == from && known_to == to)
            {
                pairs.push((from.to_string(), to.to_string()));
            }
        }
        pairs
    }

    /// Whether a later relationship joins the same two elements in the same
    /// direction as the one at `index`. `merman` keeps one relationship per
    /// ordered pair, drawn with the last one's kind and texts.
    pub fn relation_is_replaced_later(&self, index: usize) -> bool {
        let Some(C4Item::Relation(statement)) = self.items.get(index) else {
            return false;
        };
        let Some(ends) = statement.relation_ends() else {
            return false;
        };
        self.items.iter().skip(index + 1).any(
            |item| matches!(item, C4Item::Relation(later) if later.relation_ends() == Some(ends)),
        )
    }

    fn apply_order(&mut self, order: &[usize]) {
        let mut old = std::mem::take(&mut self.items)
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        self.items = order
            .iter()
            .filter_map(|index| old.get_mut(*index).and_then(Option::take))
            .collect();
    }

    /// Why the document cannot be written back as a diagram Mermaid draws the
    /// way the form shows it, in Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        let mut depth = 0usize;
        let mut titles = 0usize;
        for item in &self.items {
            match item {
                C4Item::BoundaryStart(_) => depth += 1,
                C4Item::BoundaryEnd => {
                    if depth == 0 {
                        return Some("対応する境界の無い `}` があります。".to_string());
                    }
                    depth -= 1;
                }
                C4Item::Title(title) => {
                    titles += 1;
                    if !is_valid_c4_title(title) {
                        return Some(format!(
                            "タイトル `{title}` は書けません (改行と `%%` は不可、空にもできません)。"
                        ));
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Some("閉じていない境界があります。".to_string());
        }
        if let Some(error) = self.boundary_content_error() {
            return Some(error);
        }
        if titles > 1 {
            return Some("`title` が 2 つあります。後のものだけが使われます。".to_string());
        }
        if titles == 1 && self.title_comes_from_accessible_title() {
            return Some(
                "`title` より後の `accTitle` が図のタイトルになるので、`title` は使われません。"
                    .to_string(),
            );
        }

        let mut declared: Vec<&str> = Vec::new();
        let mut elements: Vec<&str> = Vec::new();
        for item in &self.items {
            let (C4Item::Element(statement) | C4Item::BoundaryStart(statement)) = item else {
                continue;
            };
            let Some(alias) = statement.alias() else {
                return Some(format!("`{}` に別名がありません。", statement.name));
            };
            if !is_valid_c4_alias(alias) {
                return Some(format!(
                    "別名 `{alias}` は使えません (空白・`\"` `,` `(` `)` `{{` `}}` と、`$` で始まる名前は不可)。"
                ));
            }
            if declared.contains(&alias) {
                // `merman` merges two declarations of one alias into one box,
                // drawn where the first one was with what the last one says.
                return Some(format!(
                    "別名 `{alias}` が 2 回宣言されています。別の名前にしてください。"
                ));
            }
            if matches!(item, C4Item::BoundaryStart(_)) && alias == "global" {
                return Some(
                    "`global` は図全体を表す名前なので、境界の別名には使えません。".to_string(),
                );
            }
            declared.push(alias);
            if matches!(item, C4Item::Element(_)) {
                elements.push(alias);
            }
        }

        let mut pairs: Vec<(&str, &str)> = Vec::new();
        let mut styled_declarations: Vec<&str> = Vec::new();
        let mut styled_pairs: Vec<(&str, &str)> = Vec::new();
        for item in &self.items {
            match item {
                C4Item::Element(statement) | C4Item::BoundaryStart(statement) => {
                    if let Some(alias) = statement.alias()
                        && styled_declarations.contains(&alias)
                    {
                        return Some(format!(
                            "`UpdateElementStyle({alias}, …)` が `{alias}` の宣言より前にあるので効きません。宣言の後ろへ移してください。"
                        ));
                    }
                }
                C4Item::Relation(statement) => {
                    let Some((from, to)) = statement.relation_ends() else {
                        return Some(format!("`{}` の両端が読めません。", statement.name));
                    };
                    for end in [from, to] {
                        if !elements.contains(&end) {
                            return Some(if declared.contains(&end) {
                                format!(
                                    "関係の端 `{end}` は境界です。関係は要素どうしにしか引けません。"
                                )
                            } else {
                                format!("関係の端 `{end}` という要素がありません。")
                            });
                        }
                    }
                    // A second relationship between the same pair replaces the
                    // first one's kind and texts in `merman`, which the source
                    // order written back keeps; the form notes it on the row.
                    if styled_pairs.contains(&(from, to)) {
                        return Some(format!(
                            "`UpdateRelStyle({from}, {to}, …)` が関係より前にあるので効きません。関係の後ろへ移してください。"
                        ));
                    }
                    pairs.push((from, to));
                }
                C4Item::Style(statement) => match statement.name.as_str() {
                    "UpdateElementStyle" => {
                        if let Some(alias) =
                            statement.arguments.first().and_then(C4Argument::as_text)
                        {
                            styled_declarations.push(alias);
                        }
                    }
                    "UpdateRelStyle" => {
                        // Only a pair with no relationship yet goes unstyled:
                        // `merman` styles the relationship it already has, and
                        // a later one between the pair keeps that style.
                        if let (Some(from), Some(to)) = (
                            statement.arguments.first().and_then(C4Argument::as_text),
                            statement.arguments.get(1).and_then(C4Argument::as_text),
                        ) && !pairs.contains(&(from, to))
                        {
                            styled_pairs.push((from, to));
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        for statement in self.items.iter().filter_map(C4Item::statement) {
            if statement.arguments.len() < minimum_arguments(&statement.name) {
                return Some(format!("`{}` の引数が足りません。", statement.name));
            }
            for argument in &statement.arguments {
                if !is_valid_c4_argument(argument) {
                    let text = match argument {
                        C4Argument::Text { value, .. } | C4Argument::Named { value, .. } => value,
                    };
                    return Some(format!("`{text}` は書けません (`\"` と改行は使えません)。"));
                }
            }
        }
        None
    }
}

impl C4Document {
    /// `merman` 0.8 refuses a boundary with no macro inside it, and one whose
    /// first statement is a `title` or an accessibility statement, which it
    /// only takes inside a boundary after a macro. A comment counts as
    /// neither.
    fn boundary_content_error(&self) -> Option<String> {
        // For each boundary still open: its alias and whether a macro has
        // been written in it yet.
        let mut open: Vec<(&str, bool)> = Vec::new();
        for item in &self.items {
            let is_macro = match item {
                C4Item::BoundaryStart(statement) => {
                    if let Some((_, has_macro)) = open.last_mut() {
                        *has_macro = true;
                    }
                    open.push((statement.alias().unwrap_or_default(), false));
                    continue;
                }
                C4Item::BoundaryEnd => {
                    if let Some((alias, false)) = open.pop() {
                        return Some(format!(
                            "境界 `{alias}` の中が空です。Mermaid は空の境界を描けないので、要素を 1 つ以上入れるか、境界を削除してください。"
                        ));
                    }
                    continue;
                }
                C4Item::Element(_) | C4Item::Relation(_) | C4Item::Style(_) => true,
                C4Item::Title(_) => false,
                C4Item::Preserved(text) if text.starts_with("%%") => continue,
                // A relationship without a label is a macro `merman` reads and
                // leaves out of the picture.
                C4Item::Preserved(text) => !text.starts_with("acc"),
            };
            let Some((alias, has_macro)) = open.last_mut() else {
                continue;
            };
            if is_macro {
                *has_macro = true;
            } else if !*has_macro {
                return Some(format!(
                    "境界 `{alias}` の中では、`title` や `accTitle` などを最初の要素より後ろに書いてください (Mermaid が受け付けません)。"
                ));
            }
        }
        None
    }
}

/// How many arguments a macro needs for `merman` to read it as that macro.
fn minimum_arguments(name: &str) -> usize {
    if C4_RELATION_MACROS.contains(&name) {
        3 + usize::from(name == "RelIndex")
    } else if boundary_family(name) == Some(C4BoundaryFamily::Typed) {
        // `merman` inserts the boundary's type in front of the third
        // argument, which needs the first two to be there.
        2
    } else if STYLE_MACROS.contains(&name) {
        0
    } else {
        1
    }
}

fn style_names_alias(statement: &C4Statement, alias: &str) -> bool {
    statement.name == "UpdateElementStyle"
        && statement.arguments.first().and_then(C4Argument::as_text) == Some(alias)
}

fn style_names_pair(statement: &C4Statement, pairs: &[(String, String)]) -> bool {
    statement.name == "UpdateRelStyle"
        && pairs.iter().any(|(from, to)| {
            statement.arguments.first().and_then(C4Argument::as_text) == Some(from.as_str())
                && statement.arguments.get(1).and_then(C4Argument::as_text) == Some(to.as_str())
        })
}

/// Whether `source` is a C4 diagram: its first statement is `C4Context`, `C4Container`, `C4Component`, `C4Dynamic` or `C4Deployment`.
pub fn is_c4_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_c4(source: &str) -> Result<C4Document> {
    let document = scan_c4(source)?;

    // `merman` 0.8 refuses a C4 diagram with no statement at all, which is
    // what the form starts from, so a fence holding only the header and
    // comments opens as a blank diagram without asking it, the way an empty
    // fence does.
    let has_statements = document
        .items
        .iter()
        .any(|item| !matches!(item, C4Item::Preserved(text) if text.starts_with("%%")));

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if has_statements {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        let RenderSemanticModel::C4(model) = parsed.model() else {
            bail!("この図は C4 図ではありません。");
        };
        // Mermaid cuts `%%{…}%%` out of the middle of a line, and across lines,
        // before the C4 parser reads what is left, so a line this module keeps
        // as it is can still hide or add a declaration. What the form would
        // show has to be what `merman` draws.
        let shapes = model
            .shapes
            .iter()
            .map(|shape| (shape.alias.clone(), shape.parent_boundary.clone()))
            .collect::<Vec<_>>();
        let boundaries = model
            .boundaries
            .iter()
            .filter(|boundary| boundary.alias != "global")
            .map(|boundary| (boundary.alias.clone(), boundary.parent_boundary.clone()))
            .collect::<Vec<_>>();
        let relations = model
            .rels
            .iter()
            .map(|relation| (relation.from_alias.clone(), relation.to_alias.clone()))
            .collect::<Vec<_>>();
        if document.declarations_as_merman_reads_them(false) != shapes
            || document.declarations_as_merman_reads_them(true) != boundaries
            || document.relation_pairs_as_merman_reads_them() != relations
        {
            bail!(
                "`%%{{…}}%%` などの書き方のせいで、構造化エディターの読み取りが Mermaid の描く図と一致しません。テキストのまま編集してください。"
            );
        }
    }

    Ok(document)
}

fn scan_c4(source: &str) -> Result<C4Document> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = C4Document::empty();
    document.preamble = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };
    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(diagram_type) = C4DiagramType::from_keyword(header) else {
        bail!(
            "`{header}` の行は構造化編集に対応していません (`C4Context` などの見出しだけの行にしてください)。"
        );
    };
    document.diagram_type = diagram_type;

    let mut titles = 0usize;
    let mut open_boundaries = 0usize;
    let mut index = header_index + 1;
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            // Mermaid drops a `%%` line before the C4 parser sees it, and the
            // parser drops a bare `%%` as an empty comment; either way it is
            // kept as it is.
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            let text = join_lines(lines.get(start..index).unwrap_or_default())
                .trim_start()
                .to_string();
            // Mermaid cuts the directive out and reads the rest of its line as
            // a statement, which a kept line would hide from the form.
            let rest = without_directives(&text);
            if text.starts_with("%%{") && !rest.trim().is_empty() && !is_removed_comment(&rest) {
                bail!(
                    "`%%{{…}}%%` の後ろに同じ行で文が続いています ({} 行目)。指示子を 1 行に分けてから開いてください。",
                    start + 1
                );
            }
            document.items.push(C4Item::Preserved(text));
            continue;
        }

        let statement_text = strip_inline_comment(line);
        let comment = line
            .get(statement_text.len()..)
            .unwrap_or_default()
            .trim()
            .to_string();
        let statement = statement_text.trim();
        let comment_item = (!comment.is_empty()).then(|| C4Item::Preserved(comment));

        if statement == "}" {
            if open_boundaries == 0 {
                bail!(
                    "対応する境界の無い `}}` があります ({} 行目)。テキストのまま編集してください。",
                    start + 1
                );
            }
            open_boundaries -= 1;
            document.items.push(C4Item::BoundaryEnd);
            document.items.extend(comment_item);
            continue;
        }
        if let Some(title) = statement.strip_prefix("title ") {
            titles += 1;
            document.items.push(C4Item::Title(title.to_string()));
            document.items.extend(comment_item);
            continue;
        }
        if statement.starts_with("accDescription ") || is_accessible_title(statement) {
            document.items.push(C4Item::Preserved(trimmed.to_string()));
            continue;
        }
        if let Some(rest) = statement.strip_prefix("accDescr") {
            let rest = rest.trim_start();
            if rest.starts_with(':') {
                document.items.push(C4Item::Preserved(trimmed.to_string()));
                continue;
            }
            if let Some(block) = rest.strip_prefix('{') {
                if !block.contains('}') {
                    // `merman` reads on to the first line with a `}`, past the
                    // comment lines Mermaid removed before it.
                    while let Some(next) = lines.get(index).copied() {
                        index += 1;
                        if !is_removed_comment(next) && next.contains('}') {
                            break;
                        }
                    }
                }
                let mut text = trimmed.to_string();
                for line in lines.get(start + 1..index).unwrap_or_default() {
                    text.push('\n');
                    text.push_str(line.trim_end());
                }
                document.items.push(C4Item::Preserved(text));
                continue;
            }
        }
        if is_direction_statement(statement) {
            bail!(
                "`{statement}` は C4 図では使えません ({} 行目。Mermaid が受け付けません)。行を消すか、テキストのまま編集してください。",
                start + 1
            );
        }

        let Some((name, arguments_text, has_brace)) = split_macro(statement)? else {
            bail!("`{statement}` は C4 図の文として読めません。テキストのまま編集してください。");
        };
        let parsed = C4Statement {
            name: name.to_string(),
            arguments: parse_arguments(arguments_text)?,
        };

        if boundary_family(name).is_some() {
            if parsed.arguments.len() < minimum_arguments(name) {
                bail!(
                    "`{statement}` には別名とラベルの 2 つが必要です。テキストのまま編集してください。"
                );
            }
            require_alias(&parsed, statement)?;
            let mut moved_comments = Vec::new();
            if !has_brace {
                loop {
                    let Some(next) = lines.get(index).copied() else {
                        bail!("`{statement}` の後に `{{` がありません。");
                    };
                    index += 1;
                    if next.trim().is_empty() {
                        continue;
                    }
                    if is_removed_comment(next) {
                        moved_comments.push(C4Item::Preserved(next.trim().to_string()));
                        continue;
                    }
                    if next.trim() == "{" {
                        break;
                    }
                    bail!("`{statement}` の後に `{{` がありません。");
                }
            }
            open_boundaries += 1;
            document.items.push(C4Item::BoundaryStart(parsed));
            document.items.extend(comment_item);
            document.items.extend(moved_comments);
            continue;
        }
        if has_brace {
            bail!(
                "`{{` を付けられるのは境界だけです ({statement})。テキストのまま編集してください。"
            );
        }
        if element_family(name).is_some() {
            require_alias(&parsed, statement)?;
            document.items.push(C4Item::Element(parsed));
        } else if C4_RELATION_MACROS.contains(&name) {
            if parsed.arguments.len() < minimum_arguments(name) {
                // `merman` reads a relationship without a label and then
                // leaves it out, so it stays as the line it is.
                document
                    .items
                    .push(C4Item::Preserved(statement.to_string()));
            } else {
                if parsed.relation_ends().is_none() {
                    bail!("`{statement}` の両端が読めません。");
                }
                document.items.push(C4Item::Relation(parsed));
            }
        } else if STYLE_MACROS.contains(&name) {
            document.items.push(C4Item::Style(parsed));
        } else {
            bail!("`{name}` は C4 図のマクロではありません。テキストのまま編集してください。");
        }
        document.items.extend(comment_item);
    }

    // Both set the title and the last one is drawn, so a `title` that is not
    // the last would be a field whose edits change nothing.
    if titles > 1 {
        bail!(
            "`title` が 2 回書かれています。後のものだけが使われるので、1 つにまとめてからもう一度開いてください。"
        );
    }
    if titles == 1 && document.title_comes_from_accessible_title() {
        bail!(
            "`title` の後の `accTitle` が図のタイトルになっています。`accTitle` を `title` より前へ移すか、テキストのまま編集してください。"
        );
    }
    // `merman` 0.7 closed what was still open at the end of the fence; 0.8
    // refuses it.
    if open_boundaries > 0 {
        bail!("閉じていない境界があります (`}}` がありません)。テキストのまま編集してください。");
    }
    Ok(document)
}

fn require_alias(statement: &C4Statement, text: &str) -> Result<()> {
    if statement.alias().is_none() {
        bail!("`{text}` の別名が読めません (別名は最初の引数に `$…=` 無しで書きます)。");
    }
    Ok(())
}

/// Whether Mermaid removes `line` as a comment before the C4 parser reads it.
fn is_removed_comment(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("%%")
        .is_some_and(|after| !after.is_empty() && !after.starts_with('{'))
}

/// `text` with every `%%{…}%%` cut out, the way Mermaid cuts directives out
/// before a diagram's parser reads it; an unclosed one runs to the end.
fn without_directives(text: &str) -> String {
    let mut rest = String::new();
    let mut remaining = text;
    while let Some(open) = remaining.find("%%{") {
        rest.push_str(remaining.get(..open).unwrap_or_default());
        let Some(close) = remaining
            .get(open + 3..)
            .and_then(|after| after.find("}%%"))
        else {
            return rest;
        };
        remaining = remaining.get(open + 3 + close + 3..).unwrap_or_default();
    }
    rest.push_str(remaining);
    rest
}

/// `accTitle: …`, which `merman`'s C4 parser takes as the title.
fn is_accessible_title(statement: &str) -> bool {
    statement
        .strip_prefix("accTitle")
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

fn is_direction_statement(statement: &str) -> bool {
    let mut words = statement.split_whitespace();
    words.next() == Some("direction") && matches!(words.next(), Some("TB" | "BT" | "LR" | "RL"))
}

/// The line up to its `%%` comment, the way `merman` cuts it: a `%%` between
/// double quotes is text.
fn strip_inline_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && *byte == b'%' && bytes.get(index + 1) == Some(&b'%') {
            return line.get(..index).unwrap_or(line);
        }
    }
    line
}

/// Splits `Name(arguments) {` into its parts, or `None` for a line with no
/// `(` — which `merman` does not read at all.
fn split_macro(statement: &str) -> Result<Option<(&str, &str, bool)>> {
    let Some(open) = statement.find('(') else {
        return Ok(None);
    };
    let name = statement.get(..open).unwrap_or_default().trim();
    if name.is_empty() {
        return Ok(None);
    }
    let after = statement.get(open + 1..).unwrap_or_default();
    let Some(close) = after.rfind(')') else {
        bail!("`{statement}` の `)` がありません。");
    };
    let arguments = after.get(..close).unwrap_or_default();
    let rest = after.get(close + 1..).unwrap_or_default().trim();
    let has_brace = match rest.strip_prefix('{') {
        Some(after_brace) if after_brace.trim().is_empty() => true,
        Some(_) => bail!("`{statement}` の `{{` の後ろには何も書けません。"),
        None if rest.is_empty() => false,
        None => bail!("`{statement}` の `)` の後ろの `{rest}` は読めません。"),
    };
    Ok(Some((name, arguments, has_brace)))
}

/// Splits the argument list at the commas outside double quotes, the way
/// `merman` does: an empty argument in the middle counts, while nothing after
/// the last comma does.
fn parse_arguments(input: &str) -> Result<Vec<C4Argument>> {
    let mut arguments = Vec::new();
    let mut rest = input;
    while !rest.trim().is_empty() {
        let mut in_quotes = false;
        let mut split = None;
        for (offset, character) in rest.char_indices() {
            match character {
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => {
                    split = Some(offset);
                    break;
                }
                _ => {}
            }
        }
        let segment = match split {
            Some(offset) => rest.get(..offset).unwrap_or_default(),
            None => rest,
        };
        arguments.push(parse_argument(segment.trim())?);
        match split.and_then(|offset| rest.get(offset + 1..)) {
            Some(next) => rest = next,
            None => break,
        }
    }
    Ok(arguments)
}

fn parse_argument(segment: &str) -> Result<C4Argument> {
    if segment.is_empty() {
        return Ok(C4Argument::text(""));
    }
    if let Some(rest) = segment.strip_prefix('$') {
        let Some(equals) = rest.find('=') else {
            bail!("`{segment}` は `$名前=\"値\"` の形になっていません。");
        };
        let key = rest.get(..equals).unwrap_or_default().trim();
        if key.is_empty() {
            bail!("`{segment}` に名前がありません。");
        }
        let value = parse_quoted(rest.get(equals + 1..).unwrap_or_default())?;
        return Ok(C4Argument::Named {
            key: key.to_string(),
            value,
        });
    }
    if segment.starts_with('"') {
        return Ok(C4Argument::text(&parse_quoted(segment)?));
    }
    Ok(C4Argument::bare(segment))
}

fn parse_quoted(input: &str) -> Result<String> {
    let input = input.trim();
    let Some(rest) = input.strip_prefix('"') else {
        bail!("`{input}` は `\"` で囲まれていません。");
    };
    let Some(end) = rest.find('"') else {
        bail!("`{input}` の `\"` が閉じていません。");
    };
    let trailing = rest.get(end + 1..).unwrap_or_default().trim();
    if !trailing.is_empty() {
        bail!("`{input}` の `\"` の後ろの `{trailing}` は読めません。");
    }
    Ok(rest.get(..end).unwrap_or_default().to_string())
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_c4(document: &C4Document) -> String {
    let mut out = String::new();
    for line in &document.preamble {
        out.push_str(&line.text);
        out.push('\n');
    }
    out.push_str(document.diagram_type.keyword());
    out.push('\n');

    let mut depth = 1usize;
    for item in &document.items {
        if matches!(item, C4Item::BoundaryEnd) {
            depth = depth.saturating_sub(1).max(1);
        }
        out.push_str(&INDENT.repeat(depth));
        match item {
            C4Item::Title(title) => {
                out.push_str("title ");
                out.push_str(title);
            }
            C4Item::Element(statement) | C4Item::Relation(statement) | C4Item::Style(statement) => {
                push_statement(&mut out, statement);
            }
            C4Item::BoundaryStart(statement) => {
                push_statement(&mut out, statement);
                out.push_str(" {");
                depth += 1;
            }
            C4Item::BoundaryEnd => out.push('}'),
            C4Item::Preserved(text) => out.push_str(text),
        }
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn push_statement(out: &mut String, statement: &C4Statement) {
    out.push_str(&statement.name);
    out.push('(');
    let padding = C4Argument::text("");
    let mut arguments = statement.arguments.iter().collect::<Vec<_>>();
    // `merman` cannot read a typed boundary with fewer than two arguments, and
    // an empty label is what it would draw for the missing one.
    while boundary_family(&statement.name) == Some(C4BoundaryFamily::Typed) && arguments.len() < 2 {
        arguments.push(&padding);
    }
    for (index, argument) in arguments.into_iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        match argument {
            C4Argument::Text { value, quoted } if *quoted || value.is_empty() => {
                out.push('"');
                out.push_str(value);
                out.push('"');
            }
            C4Argument::Text { value, .. } => out.push_str(value),
            C4Argument::Named { key, value } => {
                out.push('$');
                out.push_str(key);
                out.push_str("=\"");
                out.push_str(value);
                out.push('"');
            }
        }
    }
    out.push(')');
}

/// Whether `alias` can name an element or a boundary: it is written bare, so
/// it cannot hold what ends an argument or a macro call.
pub fn is_valid_c4_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('$')
        && !alias.contains("%%")
        && !alias.chars().any(|character| {
            character.is_whitespace() || matches!(character, '"' | ',' | '(' | ')' | '{' | '}')
        })
}

/// Whether `text` can be written between double quotes. `merman` ends the
/// text at the next `"` and has no escape for one.
pub fn is_valid_c4_text(text: &str) -> bool {
    !text.contains(['"', '\n', '\r'])
}

/// Whether `title` can be written after `title`. It runs to the end of the
/// line, where a `%%` would cut it short.
pub fn is_valid_c4_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.trim_end() == title
        && !title.contains(['\n', '\r'])
        && !title.contains("%%")
}

fn is_valid_c4_argument(argument: &C4Argument) -> bool {
    match argument {
        C4Argument::Text {
            value,
            quoted: true,
        } => is_valid_c4_text(value),
        // A bare text is only ever what the parser read, so it is written back
        // as it came; it just has to stay one argument on one line.
        C4Argument::Text {
            value,
            quoted: false,
        } => {
            value.trim() == value && !value.contains(['\n', '\r']) && !value.starts_with(['$', '"'])
        }
        C4Argument::Named { key, value } => {
            !key.is_empty()
                && key.trim() == key
                && !key.contains(['=', ',', '"', '\n', '\r'])
                && is_valid_c4_text(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    const TEMPLATE: &str = "C4Context\n    %% Person は利用者、System は自システム、System_Ext は外部、Rel は関係\n    title 受注システムの全体像\n    Person(customer, \"顧客\", \"商品を注文する\")\n    System(shop, \"受注システム\", \"注文を受け付ける\")\n    System_Ext(mail, \"メール配信\", \"通知を送る\")\n    Rel(customer, shop, \"注文する\")\n    Rel(shop, mail, \"通知を依頼する\")";

    fn parse(source: &str) -> C4Document {
        parse_c4(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_c4(&first);
        let second = parse(&first_text);
        let second_text = emit_c4(&second);
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

    /// What `merman` itself reads from `source`: the shapes, boundaries,
    /// relationships, title and layout, as JSON.
    fn merman_model(source: &str) -> serde_json::Value {
        Engine::new()
            .parse_diagram_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source}: {error}"))
            .unwrap_or_else(|| panic!("merman should find a diagram in {source}"))
            .model
    }

    /// The round trip changes nothing `merman` reads.
    fn assert_same_meaning(before: &str, after: &str) {
        assert_eq!(
            merman_model(before),
            merman_model(after),
            "the meaning changed:\n{before}\n---\n{after}"
        );
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    fn statement_at_mut(document: &mut C4Document, index: usize) -> &mut C4Statement {
        match document.items.get_mut(index) {
            Some(
                C4Item::Element(statement)
                | C4Item::BoundaryStart(statement)
                | C4Item::Relation(statement)
                | C4Item::Style(statement),
            ) => statement,
            other => panic!("item {index} should be a macro call, got {other:?}"),
        }
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in [
            "C4Context\n    title x",
            "C4Container",
            "C4Component",
            "C4Dynamic",
            "C4Deployment\n    title x",
        ] {
            assert!(is_c4_source(source), "{source} should be a C4 diagram");
        }
        for source in [
            "C4\n  title x",
            "C4Contexts",
            "c4context",
            "C4Containers",
            "",
            "%% c4",
            "flowchart TD\n    A --> B",
        ] {
            assert!(!is_c4_source(source), "{source} should not be a C4 diagram");
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(document.diagram_type, C4DiagramType::Context);
        assert_eq!(document.title(), Some("受注システムの全体像"));
        assert_eq!(document.element_aliases(), vec!["customer", "shop", "mail"]);
        assert_eq!(emit_c4(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_context_diagram() {
        assert_eq!(parse(""), C4Document::empty());
        assert_eq!(parse("C4Context"), C4Document::empty());
        assert_eq!(emit_c4(&C4Document::empty()), "C4Context");
        assert_eq!(C4Document::empty().structure_error(), None);
        // `merman` 0.8 no longer draws a C4 diagram with no statement in it,
        // so the blank diagram opens without asking it (the editor asks for
        // an element before it writes the fence), comments and all.
        assert!(render_to_svg("C4Context", &MermaidTheme::default()).is_err());
        let commented = parse("C4Container\n    %% メモ");
        assert_eq!(commented.diagram_type, C4DiagramType::Container);
        assert_eq!(emit_c4(&commented), "C4Container\n    %% メモ");
        // A title is a statement, so a diagram with one is read by `merman`.
        assert_eq!(parse("C4Context\n    title 題").title(), Some("題"));
    }

    /// Everything the parser rewrites, and everything it keeps where it was.
    #[test]
    fn boundaries_comments_and_style_updates_keep_their_place() {
        let source = "---\ntitle: 図\n---\n%%{wrap}%%\nC4Deployment\n  %% 先頭のメモ\n  accTitle: 配置\n  accDescr {\n    説明の 1 行目\n      2 行目\n  }\n  Deployment_Node(dc, \"データセンター\", \"Ubuntu\")\n  %% 括弧の前のメモ\n  {\n    Node_L(app, \"アプリ\") {\n      Container(api, \"API\",\"Rust\" , \"注文を受ける\") %% 行末のメモ\n      ContainerDb(db, \"DB\", \"Postgres\", \"50%% の容量\", $tags=\"primary\")\n    } %% 閉じのメモ\n  }\n  Person(user, , \"利用者\")\n  Rel(user, api, \"使う\", \"HTTPS\")\n  RelIndex(1, api, db, \"読み書き\")\n  Rel(api, user)\n  UpdateRelStyle(user, api, $offsetY=\"-40\")\n  UpdateElementStyle(api, $bgColor=\"red\")\n  UpdateLayoutConfig($c4ShapeInRow=\"2\")";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 図\n---\n%%{wrap}%%\nC4Deployment\n    %% 先頭のメモ\n    accTitle: 配置\n    accDescr {\n    説明の 1 行目\n      2 行目\n  }\n    Deployment_Node(dc, \"データセンター\", \"Ubuntu\") {\n        %% 括弧の前のメモ\n        Node_L(app, \"アプリ\") {\n            Container(api, \"API\", \"Rust\", \"注文を受ける\")\n            %% 行末のメモ\n            ContainerDb(db, \"DB\", \"Postgres\", \"50%% の容量\", $tags=\"primary\")\n        }\n        %% 閉じのメモ\n    }\n    Person(user, \"\", \"利用者\")\n    Rel(user, api, \"使う\", \"HTTPS\")\n    RelIndex(1, api, db, \"読み書き\")\n    Rel(api, user)\n    UpdateRelStyle(user, api, $offsetY=\"-40\")\n    UpdateElementStyle(api, $bgColor=\"red\")\n    UpdateLayoutConfig($c4ShapeInRow=\"2\")"
        );
        assert_same_meaning(source, &emitted);
        assert_renders(&emitted);

        let document = parse(source);
        assert_eq!(document.structure_error(), None);
        assert_eq!(document.title(), None);
        assert!(document.title_comes_from_accessible_title());
        let mut titled = document.clone();
        titled.set_title(Some("題"));
        assert_eq!(titled.structure_error(), None);
        assert!(!titled.title_comes_from_accessible_title());
        assert!(matches!(titled.items.get(2), Some(C4Item::Title(_))));
        // The relationship without a label is read and ignored by `merman`,
        // so it is kept as a line rather than offered as a relationship.
        assert!(
            document
                .items
                .contains(&C4Item::Preserved("Rel(api, user)".to_string()))
        );
        let start = document
            .items
            .iter()
            .position(|item| matches!(item, C4Item::BoundaryStart(statement) if statement.alias() == Some("app")))
            .expect("the inner node should be a boundary");
        assert_eq!(document.depth(start), 1);
        assert_eq!(document.parent(start + 1), Some(start));
        assert_eq!(document.depth(start + 1), 2);
    }

    /// `merman` 0.7 closed a boundary left open at the end of the fence, and
    /// the model closed it the same way; 0.8 refuses the diagram, so the
    /// model does not open it either.
    #[test]
    fn a_boundary_left_open_is_refused() {
        let source = "C4Context\n    Boundary(b, \"B\") {\n    Person(p, \"P\")";
        assert!(render_to_svg(source, &MermaidTheme::default()).is_err());
        let error = parse_c4(source).expect_err("an open boundary should be refused");
        assert!(error.to_string().contains("閉じていない境界"), "{error}");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "C4Context  \r\n  title 全体  \r\n  Person(a, \"A\")  \r\n";
        assert_eq!(
            emit_c4(&parse(source)),
            "C4Context\n    title 全体\n    Person(a, \"A\")"
        );
    }

    #[test]
    fn constructs_that_would_change_meaning_are_refused() {
        for (source, expected) in [
            ("C4Context\n    title a\n    title b", "2 回"),
            ("C4Context\n    title a\n    accTitle: b", "accTitle"),
            ("C4Context\n    }", "境界の無い"),
            ("C4Context\n    Person(a, \"A\") {\n    }", "境界だけ"),
            ("C4Context\n    Actor(a, \"A\")", "マクロではありません"),
            ("C4Context\n    something", "読めません"),
            ("C4Context\n    System_Boundary(b) {\n    }", "2 つ"),
            (
                "C4Context\n    Boundary(b, \"B\")\n    Person(a, \"A\")",
                "`{`",
            ),
            ("C4Context\n    Person($label=\"x\", \"A\")", "別名"),
            ("C4Context\n    Person(a, \"A\" x)", "読めません"),
            ("C4Context\n    Person(a, $x)", "$名前"),
            ("C4Context title x", "見出し"),
            // `merman` 0.8 refuses `direction` in a C4 diagram.
            (
                "C4Context\n    direction LR\n    Person(a, \"A\")",
                "C4 図では使えません",
            ),
        ] {
            let error = parse_c4(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }
    }

    #[test]
    fn what_merman_draws_differently_is_reported() {
        for (source, expected) in [
            (
                "C4Context\n    Person(a, \"A\")\n    Person(a, \"B\")",
                "2 回宣言",
            ),
            (
                "C4Context\n    Person(a, \"A\")\n    Boundary(a, \"B\") {\n        Person(c, \"C\")\n    }",
                "2 回宣言",
            ),
            (
                "C4Context\n    Boundary(global, \"B\") {\n        Person(c, \"C\")\n    }",
                "global",
            ),
            (
                "C4Context\n    Person(a, \"A\")\n    Rel(a, b, \"x\")",
                "要素がありません",
            ),
            (
                "C4Context\n    Person(a, \"A\")\n    Boundary(b, \"B\") {\n        Person(c, \"C\")\n    }\n    Rel(a, b, \"x\")",
                "境界です",
            ),
            (
                "C4Context\n    UpdateElementStyle(a, $bgColor=\"red\")\n    Person(a, \"A\")",
                "宣言より前",
            ),
            (
                "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    UpdateRelStyle(a, b, $textColor=\"red\")\n    Rel(a, b, \"x\")",
                "関係より前",
            ),
            ("C4Context\n    Person(\"a b\", \"A\")", "別名"),
        ] {
            let document = parse(source);
            let error = document
                .structure_error()
                .unwrap_or_else(|| panic!("`{source}` should be reported"));
            assert!(
                error.contains(expected),
                "the reason for `{source}` should mention {expected}: {error}"
            );
        }

        // `merman` 0.8 refuses an empty boundary, and a boundary whose first
        // statement is a title or an accessibility statement.
        let mut document = parse(TEMPLATE);
        document.insert_boundary(
            document.insertion_point(None),
            C4Statement::declaration("Boundary", "b1", "境界"),
        );
        let error = document.structure_error().expect("an empty boundary");
        assert!(error.contains("`b1` の中が空"), "{error}");
        assert!(render_to_svg(&emit_c4(&document), &MermaidTheme::default()).is_err());
        let start = document.insertion_point(None) - 2;
        document
            .items
            .insert(start + 1, C4Item::Title("題".to_string()));
        document.items.insert(
            start + 2,
            C4Item::Element(C4Statement::declaration("Person", "p", "P")),
        );
        let error = document
            .structure_error()
            .expect("a title before the element");
        assert!(error.contains("最初の要素より後ろ"), "{error}");
        assert!(render_to_svg(&emit_c4(&document), &MermaidTheme::default()).is_err());
        document.items.swap(start + 1, start + 2);
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("2 つ"))
        );
        document.items.remove(start + 2);
        assert_eq!(document.structure_error(), None);
        assert_renders(&emit_c4(&document));
        for comment in ["%% メモ", "Rel(customer, shop)"] {
            let mut commented = document.clone();
            commented
                .items
                .insert(start + 1, C4Item::Preserved(comment.to_string()));
            assert_eq!(commented.structure_error(), None, "{comment}");
            assert_renders(&emit_c4(&commented));
        }
        let mut accessible = document.clone();
        accessible
            .items
            .insert(start + 1, C4Item::Preserved("accTitle: t".to_string()));
        assert!(accessible.structure_error().is_some());

        let mut document = parse(TEMPLATE);
        document.set_title(Some("a %% b"));
        assert!(document.structure_error().is_some());
        document.set_title(Some("題"));
        assert_eq!(document.structure_error(), None);
        statement_at_mut(&mut document, 3).set_field(C4Field::Label, "引用 \" 符");
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("`\"`"))
        );
    }

    /// Mermaid cuts a directive out of its line before the C4 parser reads the
    /// rest, and a comment that opens one swallows every line up to its end;
    /// either way the form would show something other than what is drawn.
    #[test]
    fn a_directive_that_hides_or_adds_a_statement_is_refused() {
        for (source, expected) in [
            (
                "C4Context\n    %%{wrap}%% Person(x, \"X\")\n    Person(a, \"A\")",
                "同じ行",
            ),
            (
                "C4Context\n    Boundary(b, \"B\") {\n    Person(a, \"A\")\n    %%{wrap}%% }\n    Person(c, \"C\")",
                "同じ行",
            ),
            (
                "C4Context\n    Person(a, \"A\")\n    %%{init: {\n    }}%% UpdateElementStyle(a, $bgColor=\"red\")",
                "同じ行",
            ),
            (
                "C4Context\n    Person(a, \"A\")\n    %% メモ %%{\n    Person(b, \"B\")\n    %% }%%",
                "一致しません",
            ),
        ] {
            let error = parse_c4(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }

        for source in [
            "C4Context\n    %%{wrap}%%\n    Person(a, \"A\")",
            "C4Context\n    %%{wrap}%% %% メモ\n    Person(a, \"A\")",
            "C4Context\n    %%{init: {\n    }}%%\n    Person(a, \"A\")",
        ] {
            let emitted = assert_stable(source);
            assert_same_meaning(source, &emitted);
        }
    }

    /// `merman` keeps one relationship per ordered pair, placed where the
    /// first was and drawn with the last one's kind and texts; the source
    /// order written back keeps that, so the diagram still opens and confirms.
    #[test]
    fn a_second_relationship_between_the_same_pair_is_kept_in_order() {
        let source = "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Rel(a, b, \"x\", $tags=\"t\")\n    UpdateRelStyle(a, b, $textColor=\"red\")\n    Rel_Back(a, b, \"y\")\n    Rel(b, a, \"z\")";
        let document = parse(source);
        assert_eq!(document.structure_error(), None);
        assert!(document.relation_is_replaced_later(2));
        assert!(!document.relation_is_replaced_later(4));
        assert!(!document.relation_is_replaced_later(5));
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_same_meaning(source, &emitted);
        assert_renders(&emitted);

        // The style stays while a relationship between the pair above it is
        // left for it to style, and goes when none is.
        let mut document = parse(source);
        let order = document.remove_relation(4);
        assert_eq!(order, vec![0, 1, 2, 3, 5]);
        assert_eq!(
            emit_c4(&document),
            "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Rel(a, b, \"x\", $tags=\"t\")\n    UpdateRelStyle(a, b, $textColor=\"red\")\n    Rel(b, a, \"z\")"
        );
        assert_eq!(document.structure_error(), None);

        let mut document = parse(source);
        let order = document.remove_relation(2);
        assert_eq!(order, vec![0, 1, 4, 5]);
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Rel_Back(a, b, \"y\")\n    Rel(b, a, \"z\")"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);
    }

    #[test]
    fn a_rename_reaches_relationships_and_style_updates() {
        let source = "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Rel(a, b, \"x\")\n    RelIndex(2, b, a, \"y\")\n    UpdateElementStyle(a, $bgColor=\"red\")\n    UpdateRelStyle(a, b, $textColor=\"red\")";
        let mut document = parse(source);
        statement_at_mut(&mut document, 0).set_alias("b");
        statement_at_mut(&mut document, 1).set_alias("a");
        document.rename_references(&[
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ]);
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Context\n    Person(b, \"A\")\n    Person(a, \"B\")\n    Rel(b, a, \"x\")\n    RelIndex(2, a, b, \"y\")\n    UpdateElementStyle(b, $bgColor=\"red\")\n    UpdateRelStyle(b, a, $textColor=\"red\")"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);

        // An alias that cannot be written bare is quoted, so that the call
        // still reads as the same arguments while the form reports it.
        statement_at_mut(&mut document, 0).set_alias("b c");
        assert!(emit_c4(&document).contains("Person(\"b c\", \"A\")"));
        assert!(document.structure_error().is_some());
    }

    #[test]
    fn removing_an_element_takes_its_relationships_and_styles_with_it() {
        let source = "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Person(c, \"C\")\n    Rel(a, b, \"x\")\n    Rel(b, c, \"y\")\n    UpdateElementStyle(a, $bgColor=\"red\")\n    UpdateRelStyle(a, b, $textColor=\"red\")\n    UpdateRelStyle(b, c, $textColor=\"blue\")";
        let mut document = parse(source);
        let order = document.remove_element(0);
        assert_eq!(order, vec![1, 2, 4, 7]);
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Context\n    Person(b, \"B\")\n    Person(c, \"C\")\n    Rel(b, c, \"y\")\n    UpdateRelStyle(b, c, $textColor=\"blue\")"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);

        let order = document.remove_relation(2);
        assert_eq!(order, vec![0, 1]);
        assert_eq!(
            emit_c4(&document),
            "C4Context\n    Person(b, \"B\")\n    Person(c, \"C\")"
        );
    }

    #[test]
    fn removing_a_boundary_keeps_what_was_inside_it() {
        let source = "C4Context\n    Enterprise_Boundary(e, \"E\") {\n        System_Boundary(s, \"S\") {\n            Person(a, \"A\")\n        }\n        Person(b, \"B\")\n    }\n    UpdateElementStyle(s, $bgColor=\"red\")";
        let mut document = parse(source);
        let order = document.remove_boundary(1);
        assert_eq!(order, vec![0, 2, 4, 5]);
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Context\n    Enterprise_Boundary(e, \"E\") {\n        Person(a, \"A\")\n        Person(b, \"B\")\n    }"
        );
        assert_same_meaning(
            &emitted,
            "C4Context\nEnterprise_Boundary(e, \"E\") {\nPerson(a, \"A\")\nPerson(b, \"B\")\n}",
        );
    }

    /// An element steps over the element or boundary edge next to it, which
    /// is how it moves into and out of boundaries; a boundary moves with
    /// everything inside it.
    #[test]
    fn elements_and_boundaries_move_one_step_at_a_time() {
        let source = "C4Context
    title t
    Person(a, \"A\")
    Boundary(b, \"B\") {
        Person(c, \"C\")
    }
    Rel(a, c, \"x\")";
        let mut document = parse(source);
        assert!(!document.can_move_in_tree(1, false));
        let order = document.move_in_tree(1, true);
        assert_eq!(order, vec![0, 2, 1, 3, 4, 5]);
        assert_eq!(
            emit_c4(&document),
            "C4Context
    title t
    Boundary(b, \"B\") {
        Person(a, \"A\")
        Person(c, \"C\")
    }
    Rel(a, c, \"x\")"
        );
        let order = document.move_in_tree(3, true);
        assert_eq!(order, vec![0, 1, 2, 4, 3, 5]);
        assert!(!document.can_move_in_tree(4, true));
        let order = document.move_in_tree(4, false);
        assert_eq!(order, vec![0, 1, 2, 4, 3, 5]);
        let order = document.move_in_tree(2, false);
        assert_eq!(order, vec![0, 2, 1, 3, 4, 5]);
        // The boundary, with what is inside it, steps over the element above.
        let order = document.move_in_tree(2, false);
        assert_eq!(order, vec![0, 2, 3, 4, 1, 5]);
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Context
    title t
    Boundary(b, \"B\") {
        Person(c, \"C\")
    }
    Person(a, \"A\")
    Rel(a, c, \"x\")"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);
    }

    #[test]
    fn an_element_moves_into_a_boundary_and_back_out() {
        let source = "C4Context\n    Person(a, \"A\")\n    Boundary(b, \"B\") {\n        Person(c, \"C\")\n    }\n    Boundary(d, \"D\") {\n        Person(e, \"E\")\n    }\n    Rel(a, c, \"x\")";
        let mut document = parse(source);
        let order = document.move_element_to(0, Some(4));
        assert_eq!(order, vec![1, 2, 3, 4, 5, 0, 6, 7]);
        assert_eq!(document.parent(5), Some(3));
        let order = document.move_element_to(5, None);
        assert_eq!(order, vec![0, 1, 2, 3, 4, 6, 5, 7]);
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Context\n    Boundary(b, \"B\") {\n        Person(c, \"C\")\n    }\n    Boundary(d, \"D\") {\n        Person(e, \"E\")\n    }\n    Person(a, \"A\")\n    Rel(a, c, \"x\")"
        );
        assert_renders(&emitted);
        // Moving the last element out of a boundary leaves it empty, which
        // `merman` 0.8 refuses, so it is reported until something goes back.
        let order = document.move_element_to(4, None);
        assert_eq!(order, vec![0, 1, 2, 3, 5, 6, 4, 7]);
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("`d` の中が空"))
        );

        let mut document = parse("C4Context\n    Rel(a, b, \"x\")");
        assert_eq!(document.insertion_point(None), 0);
        document.insert_boundary(0, C4Statement::declaration("Boundary", "b1", "境界"));
        assert_eq!(document.insertion_point(Some(0)), 1);
        assert_eq!(document.insertion_point(None), 2);
        assert_eq!(document.unused_alias("b"), "b2");
    }

    #[test]
    fn relationships_trade_places_with_each_other_only() {
        let source = "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Rel(a, b, \"x\")\n    Person(c, \"C\")\n    Rel(b, c, \"y\")";
        let mut document = parse(source);
        assert!(!document.can_move_relation(2, false));
        assert!(!document.can_move_relation(0, true));
        let order = document.move_relation(2, true);
        assert_eq!(order, vec![0, 1, 4, 3, 2]);
        assert_eq!(
            emit_c4(&document),
            "C4Context\n    Person(a, \"A\")\n    Person(b, \"B\")\n    Rel(b, c, \"y\")\n    Person(c, \"C\")\n    Rel(a, b, \"x\")"
        );
    }

    #[test]
    fn fields_are_read_and_written_at_the_position_the_macro_gives_them() {
        let mut document = parse(
            "C4Deployment
    Person(p, \"P\")
    Container(c, \"C\", $descr=\"named\")
    Node(n, \"N\") {
        Container(nc, \"NC\")
    }
    Boundary(b, \"B\") {
        Container(bc, \"BC\")
    }
    Rel(p, c, \"x\")",
        );
        let person = statement_at_mut(&mut document, 0);
        assert_eq!(person.field(C4Field::Technology), C4FieldValue::Unavailable);
        assert_eq!(
            person.field(C4Field::Description),
            C4FieldValue::Value(String::new())
        );
        person.set_field(C4Field::Description, "説明");
        // The named description sits where the technology goes, so neither
        // text can be edited without changing what the other means.
        let container = statement_at_mut(&mut document, 1);
        assert_eq!(container.field(C4Field::Description), C4FieldValue::Locked);
        assert_eq!(container.field(C4Field::Technology), C4FieldValue::Locked);
        container.set_field(C4Field::Description, "無視される");
        container.set_field(C4Field::Technology, "無視される");
        let node = statement_at_mut(&mut document, 2);
        assert_eq!(
            node.field(C4Field::Type),
            C4FieldValue::Value("node".to_string())
        );
        node.set_field(C4Field::Type, "node");
        node.set_field(C4Field::Description, "サーバー");
        let boundary = statement_at_mut(&mut document, 5);
        assert_eq!(
            boundary.field(C4Field::Type),
            C4FieldValue::Value("system".to_string())
        );
        assert_eq!(
            boundary.field(C4Field::Description),
            C4FieldValue::Unavailable
        );
        let relation = statement_at_mut(&mut document, 8);
        relation.set_field(C4Field::Description, "説明");
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Deployment
    Person(p, \"P\", \"説明\")
    Container(c, \"C\", $descr=\"named\")
    Node(n, \"N\", \"node\", \"サーバー\") {
        Container(nc, \"NC\")
    }
    Boundary(b, \"B\") {
        Container(bc, \"BC\")
    }
    Rel(p, c, \"x\", \"\", \"説明\")"
        );
        assert_renders(&emitted);
        // The defaults written in between are what `merman` drew without them.
        let model = merman_model(&emitted);
        assert_eq!(
            model["boundaries"][1]["type"]["text"],
            serde_json::json!("node")
        );
        assert_eq!(
            model["boundaries"][1]["descr"]["text"],
            serde_json::json!("サーバー")
        );
        // `merman` reads the named description into the technology's place
        // and then writes the unwritten description position over it, which
        // is why the form leaves both alone.
        assert_eq!(model["shapes"][1]["descr"]["text"], serde_json::json!(""));
        assert_eq!(model["rels"][0]["techn"]["text"], serde_json::json!(""));
        assert_eq!(model["rels"][0]["descr"]["text"], serde_json::json!("説明"));
    }

    #[test]
    fn a_macro_change_moves_the_arguments_to_where_the_new_macro_reads_them() {
        let source = "C4Container\n    Person(p, \"P\", \"説明\", \"users\")\n    Container(c, \"C\", \"Rust\", \"説明\")\n    Container(d, \"D\", \"\", \"説明\", $tags=\"x\")\n    Deployment_Node(n, \"N\") {\n        Container(e, \"E\")\n    }\n    RelIndex(1, p, c, \"x\")";
        let mut document = parse(source);
        let person = statement_at_mut(&mut document, 0);
        assert!(person.can_become("Container"));
        person.set_macro("Container");
        let container = statement_at_mut(&mut document, 1);
        assert!(!container.can_become("System"));
        container.set_macro("System");
        assert_eq!(container.name, "Container");
        container.set_macro("ComponentDb_Ext");
        let empty_technology = statement_at_mut(&mut document, 2);
        assert!(empty_technology.can_become("Person_Ext"));
        empty_technology.set_macro("Person_Ext");
        let node = statement_at_mut(&mut document, 3);
        assert!(!node.can_become("Boundary"));
        assert!(node.can_become("Node_R"));
        node.set_macro("Node_R");
        let relation = statement_at_mut(&mut document, 6);
        assert!(!relation.can_become("RelIndex"));
        relation.set_macro("Rel_Back");
        let emitted = emit_c4(&document);
        assert_eq!(
            emitted,
            "C4Container\n    Container(p, \"P\", \"\", \"説明\", \"users\")\n    ComponentDb_Ext(c, \"C\", \"Rust\", \"説明\")\n    Person_Ext(d, \"D\", \"説明\", $tags=\"x\")\n    Node_R(n, \"N\") {\n        Container(e, \"E\")\n    }\n    Rel_Back(p, c, \"x\")"
        );
        let model = merman_model(&emitted);
        assert_eq!(model["shapes"][0]["sprite"], serde_json::json!("users"));
        assert_eq!(
            model["shapes"][0]["descr"]["text"],
            serde_json::json!("説明")
        );
        assert_eq!(
            model["shapes"][2]["descr"]["text"],
            serde_json::json!("説明")
        );
        // `merman` 0.8, like Mermaid's own C4 database, assigns every position
        // in turn, so the unwritten tags position after `$tags` clears it:
        // the tags are drawn neither before nor after the change.
        assert_eq!(model["shapes"][2].get("tags"), None);
        assert_eq!(merman_model(source)["shapes"][2].get("tags"), None);
        assert_renders(&emitted);
    }

    #[test]
    fn values_are_validated() {
        for alias in ["a", "顧客", "api_v2", "db-1", "x.y"] {
            assert!(is_valid_c4_alias(alias), "{alias}");
        }
        for alias in ["", "a b", "a,b", "a(b", "a)", "{a", "$a", "a\"b", "a%%b"] {
            assert!(!is_valid_c4_alias(alias), "{alias}");
        }
        assert!(is_valid_c4_text("50%, (括弧), a,b {x} $y"));
        assert!(!is_valid_c4_text("a\"b"));
        assert!(!is_valid_c4_text("改行\nあり"));
        assert!(is_valid_c4_title("受注: 全体 (2026)"));
        assert!(!is_valid_c4_title(" "));
        assert!(!is_valid_c4_title("a %% b"));

        // Everything the predicates accept has to come back as itself.
        let mut document = C4Document::empty();
        document.set_title(Some("受注: 全体 (2026)"));
        document
            .items
            .push(C4Item::Element(C4Statement::declaration(
                "Container",
                "api_v2",
                "50%, (括弧), a,b {x} $y",
            )));
        document
            .items
            .push(C4Item::Element(C4Statement::declaration(
                "Person", "顧客", "",
            )));
        document.items.push(C4Item::Relation(C4Statement::relation(
            "Rel",
            "顧客",
            "api_v2",
            "使う, 毎日",
        )));
        assert_eq!(document.structure_error(), None);
        let emitted = emit_c4(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_renders(&emitted);
    }

    /// Every C4 example `merman` is tested with: what opens must round trip to
    /// the same `merman` model and still draw, and what does not open must say
    /// why.
    #[test]
    fn merman_examples_round_trip() {
        let Some(directory) = crate::merman_fixture_directory("c4") else {
            eprintln!("merman's fixtures are not checked out; skipping");
            return;
        };
        let mut sources = std::fs::read_dir(&directory)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|extension| extension == "mmd"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        sources.sort();
        assert!(
            sources.len() > 40,
            "expected merman's C4 examples in {directory:?}"
        );

        let mut opened = 0usize;
        let mut reported = Vec::new();
        let mut refused = Vec::new();
        for path in &sources {
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            match parse_c4(&source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = assert_stable(&source);
                    assert_same_meaning(&source, &emitted);
                    if render_to_svg(&source, &MermaidTheme::default()).is_ok() {
                        assert_renders(&emitted);
                    }
                    if let Some(error) = document.structure_error() {
                        reported.push(format!("{name}: {error}"));
                    }
                }
                Err(error) => refused.push(format!("{name}: {error}")),
            }
        }
        eprintln!(
            "{opened} of {} opened; reported: {reported:#?}; refused: {refused:#?}",
            sources.len()
        );
        assert!(
            refused.len() * 10 <= sources.len(),
            "too many examples refused: {refused:#?}"
        );
    }
}
