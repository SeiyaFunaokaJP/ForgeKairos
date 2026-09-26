//! A round-trippable editing model for Mermaid mindmaps.
//!
//! The shape of this module mirrors [`crate::flowchart_model`]:
//! [`parse_mindmap`] reads the body of a ```` ```mermaid ```` fence into
//! [`MindmapDocument`], and [`emit_mindmap`] writes it back. See
//! [`crate::mermaid_model`] for the invariants every diagram kind keeps.
//!
//! A mindmap is the one kind whose structure is spelled with indentation
//! alone, and `merman` reads it relatively: a node's parent is the last node
//! above it that is indented *less*, whatever the widths are. The model keeps
//! the tree that reading produces and forgets the widths, so the emitter can
//! write every level two spaces deeper than its parent without changing which
//! node hangs under which.
//!
//! What this module refuses rather than rewrites, all of it with the reason in
//! Japanese:
//!
//! - a second root, which `merman` rejects too, and a root written on the
//!   `mindmap` line itself;
//! - a label left open to continue on the next line, which `merman` joins into
//!   one multi-line label that a one-line field cannot hold;
//! - the `(-…-)` and `-)…(-` spellings, which `merman` reads as a shape of its
//!   own but draws as the default one;
//! - a `%%` comment at the end of a node line, which `merman` drops;
//! - a `::icon(…)` or `:::class` line with nothing to attach to, a second one
//!   on the same node (the last one wins), and an icon line whose `)` is
//!   missing or is followed by more text (`merman` ignores either silently).
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the case of `::icon(`, the whitespace after `:::`, and the
//! position of comment lines between nodes, which move below the tree the way
//! they do in [`crate::gantt_model`].

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{first_statement_line, join_lines, keyword_tail, scan_preamble};

/// One level of the tree. Mermaid reads the depth relatively, so the width is
/// only a matter of looks; two spaces is what the insert template uses.
const LEVEL_INDENT: &str = "  ";

/// How a node is drawn, which is decided by the brackets around its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MindmapShape {
    /// No brackets at all: the text is both the id and the label.
    Default,
    Square,
    Rounded,
    Circle,
    Bang,
    Cloud,
    Hexagon,
}

impl MindmapShape {
    pub const ALL: [MindmapShape; 7] = [
        MindmapShape::Default,
        MindmapShape::Square,
        MindmapShape::Rounded,
        MindmapShape::Circle,
        MindmapShape::Bang,
        MindmapShape::Cloud,
        MindmapShape::Hexagon,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "標準",
            Self::Square => "四角 [ ]",
            Self::Rounded => "角丸 ( )",
            Self::Circle => "円 (( ))",
            Self::Bang => "爆発 )) ((",
            Self::Cloud => "雲 ) (",
            Self::Hexagon => "六角形 {{ }}",
        }
    }

    /// The brackets the label is written between, or `None` for the default
    /// shape, which has none.
    pub fn delimiters(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Default => None,
            Self::Square => Some(("[", "]")),
            Self::Rounded => Some(("(", ")")),
            Self::Circle => Some(("((", "))")),
            Self::Bang => Some(("))", "((")),
            Self::Cloud => Some((")", "(")),
            Self::Hexagon => Some(("{{", "}}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MindmapNode {
    /// The text written before the brackets (`root` of `root((中心))`), as the
    /// source spells it. Mermaid does not draw it or let anything refer to it,
    /// so it is only kept so that the line is written back the way it was.
    /// Always `None` for [`MindmapShape::Default`].
    pub id: Option<String>,
    /// The drawn text, without the quotes around it.
    pub label: String,
    pub shape: MindmapShape,
    /// Whether the label is written between `"` inside the brackets, which is
    /// what lets it contain the closing bracket or `%%`.
    pub quoted: bool,
    /// Whether the label is written as a Markdown string, `"` + backtick.
    pub markdown: bool,
    /// The text of a `::icon(…)` line, e.g. `fa fa-book`.
    pub icon: Option<String>,
    /// The text of a `:::` line: space separated class names.
    pub classes: Option<String>,
    pub children: Vec<MindmapNode>,
}

impl MindmapNode {
    /// A node with nothing but a label, in the default shape.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            id: None,
            label: label.into(),
            shape: MindmapShape::Default,
            quoted: false,
            markdown: false,
            icon: None,
            classes: None,
            children: Vec::new(),
        }
    }

    /// Changes the shape, dropping what only a bracketed shape can carry when
    /// the new one has no brackets, and quoting the label when the new
    /// brackets would otherwise cut it short.
    pub fn set_shape(&mut self, shape: MindmapShape) {
        self.shape = shape;
        if shape == MindmapShape::Default {
            self.id = None;
            self.quoted = false;
            self.markdown = false;
        }
        self.fit_label_quoting();
    }

    /// Quotes the label when the brackets would otherwise cut it short, and
    /// drops the quotes when they are what makes it unreadable. A label that
    /// can be written as it is keeps the spelling it already had, so an
    /// unedited node is written back byte for byte.
    pub fn fit_label_quoting(&mut self) {
        if self.shape == MindmapShape::Default || self.markdown || is_valid_mindmap_label(self) {
            return;
        }
        let mut other = self.clone();
        other.quoted = !self.quoted;
        if is_valid_mindmap_label(&other) {
            self.quoted = other.quoted;
        }
    }

    /// A copy of the node alone. Cloning the node and then clearing its
    /// children would copy the whole subtree first.
    fn without_children(&self) -> Self {
        Self {
            id: self.id.clone(),
            label: self.label.clone(),
            shape: self.shape,
            quoted: self.quoted,
            markdown: self.markdown,
            icon: self.icon.clone(),
            classes: self.classes.clone(),
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MindmapDocument {
    /// `None` for a fence with nothing but the `mindmap` line.
    pub root: Option<MindmapNode>,
    pub preserved_lines: Vec<PreservedLine>,
}

/// One node of the tree in the order the fence writes them, with its depth
/// instead of its children. The form edits the tree in this shape, because a
/// list of rows is what it draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MindmapOutlineEntry {
    /// 0 for the root, 1 for its children and so on.
    pub depth: usize,
    /// The node with its `children` left empty.
    pub node: MindmapNode,
}

impl MindmapDocument {
    /// A blank mindmap, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            root: None,
            preserved_lines: Vec::new(),
        }
    }

    /// The tree in the order the fence writes it. Walked with a stack of its
    /// own rather than by recursion, so that a deep map cannot run the thread
    /// out of stack.
    pub fn outline(&self) -> Vec<MindmapOutlineEntry> {
        let mut entries = Vec::new();
        let mut pending = self.root.iter().map(|node| (0, node)).collect::<Vec<_>>();
        while let Some((depth, node)) = pending.pop() {
            entries.push(MindmapOutlineEntry {
                depth,
                node: node.without_children(),
            });
            pending.extend(node.children.iter().rev().map(|child| (depth + 1, child)));
        }
        entries
    }

    /// The tree an outline describes. Fails when the depths do not describe
    /// one: a second root, a first entry that is not the root, or an entry
    /// more than one level below the one before it.
    pub fn from_outline(
        entries: Vec<MindmapOutlineEntry>,
        preserved_lines: Vec<PreservedLine>,
    ) -> Result<Self> {
        let mut open: Vec<MindmapNode> = Vec::new();
        for entry in entries {
            if open.is_empty() && entry.depth != 0 {
                bail!("マインドマップの最初のノードはルートでなければいけません。");
            }
            if !open.is_empty() && entry.depth == 0 {
                bail!(
                    "ルートが 2 つあります (`{}`)。マインドマップのルートは 1 つだけです。",
                    entry.node.label
                );
            }
            if entry.depth > open.len() {
                bail!(
                    "ノード `{}` が親より 2 段以上深くなっています。",
                    entry.node.label
                );
            }
            close_to(&mut open, entry.depth);
            let mut node = entry.node;
            node.children.clear();
            open.push(node);
        }
        close_to(&mut open, 1);
        Ok(Self {
            root: open.pop(),
            preserved_lines,
        })
    }

    /// Why the map cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        self.outline()
            .iter()
            .find_map(|entry| mindmap_node_problem(&entry.node))
    }
}

/// Attaches the open nodes deeper than `depth` to their parents, leaving
/// `depth` of them open.
fn close_to(open: &mut Vec<MindmapNode>, depth: usize) {
    while open.len() > depth.max(1) {
        let Some(child) = open.pop() else {
            return;
        };
        if let Some(parent) = open.last_mut() {
            parent.children.push(child);
        }
    }
}

/// Something with a depth that can be laid out as a tree in outline order. The
/// operations below move whole subtrees, so the form can apply them to its
/// rows — which carry the text fields — and the tests to plain entries.
pub trait OutlineItem {
    fn depth(&self) -> usize;
    fn set_depth(&mut self, depth: usize);
}

impl OutlineItem for MindmapOutlineEntry {
    fn depth(&self) -> usize {
        self.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }
}

/// The index just past the subtree that starts at `index`.
pub fn outline_subtree_end<T: OutlineItem>(items: &[T], index: usize) -> usize {
    let Some(depth) = items.get(index).map(OutlineItem::depth) else {
        return index;
    };
    let mut end = index + 1;
    while items.get(end).is_some_and(|item| item.depth() > depth) {
        end += 1;
    }
    end
}

/// The sibling just above the node at `index`, if it has one.
pub fn outline_previous_sibling<T: OutlineItem>(items: &[T], index: usize) -> Option<usize> {
    let depth = items.get(index)?.depth();
    let mut candidate = index;
    while candidate > 0 {
        candidate -= 1;
        let other = items.get(candidate)?.depth();
        if other == depth {
            return Some(candidate);
        }
        if other < depth {
            return None;
        }
    }
    None
}

/// The sibling just below the node at `index`, if it has one.
pub fn outline_next_sibling<T: OutlineItem>(items: &[T], index: usize) -> Option<usize> {
    let depth = items.get(index)?.depth();
    let end = outline_subtree_end(items, index);
    items
        .get(end)
        .is_some_and(|item| item.depth() == depth)
        .then_some(end)
}

/// The parent of the node at `index`, or `None` for the root.
pub fn outline_parent<T: OutlineItem>(items: &[T], index: usize) -> Option<usize> {
    let depth = items.get(index)?.depth();
    items
        .get(..index)?
        .iter()
        .rposition(|item| item.depth() < depth)
}

/// Adds `item` as the last child of the node at `index`, returning where it
/// went.
pub fn outline_add_child<T: OutlineItem>(
    items: &mut Vec<T>,
    index: usize,
    mut item: T,
) -> Option<usize> {
    let depth = items.get(index)?.depth() + 1;
    item.set_depth(depth);
    let at = outline_subtree_end(items, index);
    items.insert(at, item);
    Some(at)
}

/// Adds `item` right below the node at `index`, on the same level. The root
/// has no siblings, so it gets none.
pub fn outline_add_sibling<T: OutlineItem>(
    items: &mut Vec<T>,
    index: usize,
    mut item: T,
) -> Option<usize> {
    let depth = items.get(index)?.depth();
    if depth == 0 {
        return None;
    }
    item.set_depth(depth);
    let at = outline_subtree_end(items, index);
    items.insert(at, item);
    Some(at)
}

/// Removes the node at `index` together with everything under it. The root is
/// not removed: without it there is no map to hang anything else on.
pub fn outline_remove<T: OutlineItem>(items: &mut Vec<T>, index: usize) -> Option<Vec<T>> {
    if items.get(index)?.depth() == 0 {
        return None;
    }
    let end = outline_subtree_end(items, index);
    Some(items.drain(index..end).collect())
}

/// Swaps the node at `index` and its subtree with the sibling above or below
/// it, returning where the node went.
pub fn outline_move<T: OutlineItem>(items: &mut [T], index: usize, forward: bool) -> Option<usize> {
    if forward {
        let next = outline_next_sibling(items, index)?;
        let end = outline_subtree_end(items, next);
        items.get_mut(index..end)?.rotate_left(next - index);
        Some(index + (end - next))
    } else {
        let previous = outline_previous_sibling(items, index)?;
        let end = outline_subtree_end(items, index);
        items.get_mut(previous..end)?.rotate_left(index - previous);
        Some(previous)
    }
}

/// Makes the node at `index` the last child of the sibling above it. It
/// already sits right after that sibling's subtree, so only the depths change.
pub fn outline_indent<T: OutlineItem>(items: &mut [T], index: usize) -> bool {
    if outline_previous_sibling(items, index).is_none() {
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

/// Makes the node at `index` the sibling right after its parent, returning
/// where it went. The siblings below it stay with the parent, so nothing but
/// the node and its own subtree changes place. A child of the root cannot go
/// up a level, because that would make it a second root.
pub fn outline_outdent<T: OutlineItem>(items: &mut Vec<T>, index: usize) -> Option<usize> {
    if items.get(index)?.depth() < 2 {
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

/// Whether `source` is a mindmap. Cheap enough to call before parsing.
pub fn is_mindmap_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, "mindmap").is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_mindmap(source: &str) -> Result<MindmapDocument> {
    let document = scan_mindmap(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let engine = Engine::new();
        let parsed = engine
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::Mindmap(_)) {
            bail!("この図はマインドマップではありません。");
        }
    }

    Ok(document)
}

fn scan_mindmap(source: &str) -> Result<MindmapDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(MindmapDocument {
            root: None,
            preserved_lines,
        });
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, "mindmap") else {
        bail!("マインドマップではありません (`mindmap` で始まる図だけを構造化編集できます)。");
    };
    if !tail.is_empty() {
        bail!(
            "`mindmap` の行の `{tail}` は構造化編集に対応していません。ノードは次の行に書いてください。"
        );
    }

    // Each node with the indentation it was written at, which only says
    // something relative to the nodes around it.
    let mut written: Vec<(usize, MindmapNode)> = Vec::new();
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
            preserved_lines.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }

        match classify_line(line)? {
            BodyLine::Blank => {}
            BodyLine::Icon(icon) => {
                let Some((_, node)) = written.last_mut() else {
                    bail!("`::icon(…)` の前にノードがありません ({trimmed})。");
                };
                if node.icon.is_some() {
                    bail!(
                        "ノード `{}` に `::icon(…)` が 2 回書かれています。Mermaid は後の方だけを使うので、1 つにまとめてからもう一度開いてください。",
                        node.label
                    );
                }
                if !is_valid_mindmap_icon(&icon) {
                    bail!("アイコン `{trimmed}` は構造化編集に対応していません。");
                }
                node.icon = Some(icon);
            }
            BodyLine::Classes(classes) => {
                let Some((_, node)) = written.last_mut() else {
                    bail!("`:::` の前にノードがありません ({trimmed})。");
                };
                if node.classes.is_some() {
                    bail!(
                        "ノード `{}` に `:::` が 2 回書かれています。Mermaid は後の方だけを使うので、1 つにまとめてからもう一度開いてください。",
                        node.label
                    );
                }
                if !is_valid_mindmap_classes(&classes) {
                    bail!("クラスの無い `:::` は構造化編集に対応していません ({trimmed})。");
                }
                node.classes = Some(classes);
            }
            BodyLine::Node { indent, spec } => {
                let node = spec.into_node();
                if let Some((root_indent, _)) = written.first()
                    && indent <= *root_indent
                {
                    bail!(
                        "ルートが 2 つあります (`{}`)。マインドマップのルートは 1 つだけで、ほかのノードはルートより深く字下げします。",
                        node.label
                    );
                }
                written.push((indent, node));
                position = PreservedPosition::Bottom;
            }
        }
    }

    // A node's parent is the last node above it that is indented less, which
    // is what the stack of open indentations holds at every step.
    let mut open_indents: Vec<usize> = Vec::new();
    let mut entries = Vec::with_capacity(written.len());
    for (indent, node) in written {
        while open_indents.last().is_some_and(|open| *open >= indent) {
            open_indents.pop();
        }
        entries.push(MindmapOutlineEntry {
            depth: open_indents.len(),
            node,
        });
        open_indents.push(indent);
    }
    MindmapDocument::from_outline(entries, preserved_lines)
}

/// What one line of a mindmap body is, decided the way `merman` decides it.
enum BodyLine {
    Blank,
    Icon(String),
    Classes(String),
    Node { indent: usize, spec: NodeSpec },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeSpec {
    id: Option<String>,
    label: String,
    shape: MindmapShape,
    quoted: bool,
    markdown: bool,
}

impl NodeSpec {
    fn of(node: &MindmapNode) -> Self {
        Self {
            id: node.id.clone(),
            label: node.label.clone(),
            shape: node.shape,
            quoted: node.quoted,
            markdown: node.markdown,
        }
    }

    fn into_node(self) -> MindmapNode {
        MindmapNode {
            id: self.id,
            label: self.label,
            shape: self.shape,
            quoted: self.quoted,
            markdown: self.markdown,
            icon: None,
            classes: None,
            children: Vec::new(),
        }
    }
}

fn classify_line(line: &str) -> Result<BodyLine> {
    // `merman` counts whitespace characters, so a tab is one level of
    // indentation like a space is.
    let rest = line.trim_start();
    let indent = line
        .get(..line.len() - rest.len())
        .unwrap_or_default()
        .chars()
        .count();
    let rest = rest.trim_end();
    if rest.is_empty() {
        return Ok(BodyLine::Blank);
    }

    if rest
        .get(..ICON_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(ICON_PREFIX))
    {
        let after = rest.get(ICON_PREFIX.len()..).unwrap_or_default();
        let Some(end) = after.find(')') else {
            bail!(
                "`::icon(` の閉じ括弧がありません ({rest})。Mermaid はこの行を読み飛ばすので、閉じるかテキストのまま編集してください。"
            );
        };
        let tail = after.get(end + 1..).unwrap_or_default().trim();
        if !tail.is_empty() {
            bail!(
                "`::icon(…)` のあとの `{tail}` は Mermaid が読み飛ばします ({rest})。消すかテキストのまま編集してください。"
            );
        }
        return Ok(BodyLine::Icon(
            after.get(..end).unwrap_or_default().to_string(),
        ));
    }
    // `merman` takes everything after `:::` as class names, `%%` included.
    if let Some(after) = rest.strip_prefix(":::") {
        return Ok(BodyLine::Classes(after.trim().to_string()));
    }

    let statement = strip_inline_comment(rest).trim_end();
    if statement.len() != rest.len() {
        bail!(
            "文の末尾のコメントは構造化編集で失われます ({rest})。コメントを独立した行に移すか、テキストのまま編集してください。"
        );
    }
    let spec = parse_node_spec(statement).map_err(|error| match error {
        SpecError::Unterminated => anyhow!(
            "ノード `{statement}` のラベルが閉じていません。複数行にわたるラベルは構造化編集に対応していません。テキストのまま編集してください。"
        ),
        SpecError::UnsupportedShape(open) => anyhow!(
            "ノード `{statement}` の `{open}` の形は構造化編集に対応していません (Mermaid は標準の形で描きます)。テキストのまま編集してください。"
        ),
        SpecError::Malformed => anyhow!(
            "マインドマップのノードとして読めませんでした ({statement})。テキストのまま編集してください。"
        ),
    })?;
    Ok(BodyLine::Node { indent, spec })
}

const ICON_PREFIX: &str = "::icon(";

enum SpecError {
    Unterminated,
    UnsupportedShape(&'static str),
    Malformed,
}

/// The brackets `merman` recognizes, in the order it tries them. The first two
/// are a shape of their own in the grammar but are drawn as the default one.
const DELIMITER_PAIRS: [(&str, &str); 8] = [
    ("(-", "-)"),
    ("-)", "(-"),
    ("((", "))"),
    ("))", "(("),
    ("{{", "}}"),
    ("[", "]"),
    (")", "("),
    ("(", ")"),
];

fn delimiter_pair_at_start(input: &str) -> Option<(&'static str, &'static str)> {
    DELIMITER_PAIRS
        .into_iter()
        .find(|(start, _)| input.starts_with(start))
}

fn shape_for(start: &'static str) -> Result<MindmapShape, SpecError> {
    Ok(match start {
        "[" => MindmapShape::Square,
        "(" => MindmapShape::Rounded,
        "((" => MindmapShape::Circle,
        "))" => MindmapShape::Bang,
        ")" => MindmapShape::Cloud,
        "{{" => MindmapShape::Hexagon,
        other => return Err(SpecError::UnsupportedShape(other)),
    })
}

/// Reads a node the way `merman`'s `parse_node_spec` does.
fn parse_node_spec(input: &str) -> Result<NodeSpec, SpecError> {
    let input = input.trim_end();
    if input.is_empty() {
        return Err(SpecError::Malformed);
    }

    if let Some((start, end)) = delimiter_pair_at_start(input) {
        let (inner, tail) = extract_delimited(input, start, end)?;
        if !tail.trim().is_empty() {
            return Err(SpecError::Malformed);
        }
        let shape = shape_for(start)?;
        let (label, quoted, markdown) = unquote(inner);
        return Ok(NodeSpec {
            id: None,
            label,
            shape,
            quoted,
            markdown,
        });
    }

    let split = input.find(['(', ')', '[', '{', '}']).unwrap_or(input.len());
    let id = input.get(..split).unwrap_or_default();
    let rest = input.get(split..).unwrap_or_default().trim_end();
    if rest.is_empty() {
        return Ok(NodeSpec {
            id: None,
            label: input.to_string(),
            shape: MindmapShape::Default,
            quoted: false,
            markdown: false,
        });
    }

    let Some((start, end)) = delimiter_pair_at_start(rest) else {
        return Err(SpecError::Malformed);
    };
    let (inner, tail) = extract_delimited(rest, start, end)?;
    if !tail.trim().is_empty() {
        return Err(SpecError::Malformed);
    }
    let shape = shape_for(start)?;
    let (label, quoted, markdown) = unquote(inner);
    Ok(NodeSpec {
        id: Some(id.to_string()),
        label,
        shape,
        quoted,
        markdown,
    })
}

/// The text between `start` and the first `end` outside a quoted string, and
/// what follows it.
fn extract_delimited<'a>(
    input: &'a str,
    start: &str,
    end: &str,
) -> Result<(&'a str, &'a str), SpecError> {
    let body = input.get(start.len()..).ok_or(SpecError::Malformed)?;
    let mut in_quote = false;
    let mut in_backtick_quote = false;
    let mut characters = body.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if in_backtick_quote {
            if character == '`' && characters.peek().is_some_and(|(_, next)| *next == '"') {
                in_backtick_quote = false;
                characters.next();
            }
            continue;
        }
        if in_quote {
            if character == '"' {
                in_quote = false;
            }
            continue;
        }
        if character == '"' {
            if characters.peek().is_some_and(|(_, next)| *next == '`') {
                in_backtick_quote = true;
                characters.next();
                continue;
            }
            in_quote = true;
            continue;
        }
        let here = body.get(offset..).unwrap_or_default();
        if here.starts_with(end) {
            let inner = body.get(..offset).unwrap_or_default();
            let tail = here.get(end.len()..).unwrap_or_default();
            return Ok((inner, tail));
        }
    }
    Err(SpecError::Unterminated)
}

/// The label inside the brackets, and whether it was written as a quoted or a
/// Markdown string.
fn unquote(raw: &str) -> (String, bool, bool) {
    if let Some(inner) = raw
        .strip_prefix("\"`")
        .and_then(|inner| inner.strip_suffix("`\""))
    {
        return (inner.to_string(), false, true);
    }
    if let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        return (inner.to_string(), true, false);
    }
    (raw.to_string(), false, false)
}

/// Cuts `line` at the first `%%` outside a quoted string, the way `merman`
/// does before it reads a node.
fn strip_inline_comment(line: &str) -> &str {
    let mut in_quote = false;
    let mut in_backtick_quote = false;
    let mut characters = line.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if in_backtick_quote {
            if character == '`' && characters.peek().is_some_and(|(_, next)| *next == '"') {
                in_backtick_quote = false;
                characters.next();
            }
            continue;
        }
        if in_quote {
            if character == '"' {
                in_quote = false;
            }
            continue;
        }
        if character == '"' {
            if characters.peek().is_some_and(|(_, next)| *next == '`') {
                in_backtick_quote = true;
                characters.next();
                continue;
            }
            in_quote = true;
            continue;
        }
        if character == '%' && characters.peek().is_some_and(|(_, next)| *next == '%') {
            return line.get(..offset).unwrap_or_default();
        }
    }
    line
}

/// The node's own line, without its indentation.
fn emit_node_spec(node: &MindmapNode) -> String {
    let Some((open, close)) = node.shape.delimiters() else {
        return node.label.clone();
    };
    let label = if node.markdown {
        format!("\"`{}`\"", node.label)
    } else if node.quoted {
        format!("\"{}\"", node.label)
    } else {
        node.label.clone()
    };
    format!(
        "{}{open}{label}{close}",
        node.id.as_deref().unwrap_or_default()
    )
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_mindmap(document: &MindmapDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("mindmap\n");
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    for entry in document.outline() {
        let indent = LEVEL_INDENT.repeat(entry.depth + 1);
        out.push_str(&indent);
        out.push_str(&emit_node_spec(&entry.node));
        out.push('\n');
        // The decoration lines attach to the node above them whatever their
        // indentation; writing them at the node's own level is how Mermaid's
        // documentation writes them.
        if let Some(icon) = &entry.node.icon {
            out.push_str(&indent);
            out.push_str(ICON_PREFIX);
            out.push_str(icon);
            out.push_str(")\n");
        }
        if let Some(classes) = &entry.node.classes {
            out.push_str(&indent);
            out.push_str(":::");
            out.push_str(classes);
            out.push('\n');
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
    document: &MindmapDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether the node's line reads back as the same id, label and shape.
///
/// Which texts a label can hold depends on the brackets around it, on whether
/// it is quoted and on how `merman` scans for comments and decoration lines, so
/// rather than repeat those rules the line is written and read back.
pub fn is_valid_mindmap_label(node: &MindmapNode) -> bool {
    if node.label.trim().is_empty() || node.label.contains(['\n', '\r']) {
        return false;
    }
    if node
        .id
        .as_deref()
        .is_some_and(|id| id.contains(['\n', '\r']))
    {
        return false;
    }
    let line = format!("{LEVEL_INDENT}{}", emit_node_spec(node));
    match classify_line(&line) {
        Ok(BodyLine::Node { spec, .. }) => {
            let mut expected = NodeSpec::of(node);
            if expected.shape == MindmapShape::Default {
                expected.id = None;
                expected.quoted = false;
                expected.markdown = false;
            }
            spec == expected
        }
        _ => false,
    }
}

/// Whether `icon` can be written inside `::icon(…)`.
pub fn is_valid_mindmap_icon(icon: &str) -> bool {
    !icon.trim().is_empty() && !icon.contains([')', '\n', '\r'])
}

/// Whether `classes` can be written after `:::`.
pub fn is_valid_mindmap_classes(classes: &str) -> bool {
    !classes.trim().is_empty() && classes.trim() == classes && !classes.contains(['\n', '\r'])
}

/// The first thing wrong with one node, in Japanese.
pub fn mindmap_node_problem(node: &MindmapNode) -> Option<String> {
    if node.label.trim().is_empty() {
        return Some("ラベルが空のノードがあります。".to_string());
    }
    if node.label.contains(['\n', '\r']) {
        return Some("ノードのラベルに改行は使えません。".to_string());
    }
    if !is_valid_mindmap_label(node) {
        return Some(if node.shape == MindmapShape::Default {
            format!(
                "ラベル `{}` は標準の形では書けません (`(` `)` `[` `{{` `}}` と `%%` は使えず、前後の空白や `::icon(` `:::` で始めることもできません)。形を変えてください。",
                node.label
            )
        } else {
            format!(
                "ラベル `{}` はこの形では書けません (閉じ記号と `\"` を一緒に使うことはできません)。",
                node.label
            )
        });
    }
    if let Some(icon) = &node.icon
        && !is_valid_mindmap_icon(icon)
    {
        return Some(format!(
            "ノード `{}` のアイコン `{icon}` は書けません (`)` と改行は不可、空にもできません)。",
            node.label
        ));
    }
    if let Some(classes) = &node.classes
        && !is_valid_mindmap_classes(classes)
    {
        return Some(format!(
            "ノード `{}` のクラス `{classes}` は書けません (改行は不可、空にもできません)。",
            node.label
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str = "mindmap\n  root((中心テーマ))\n    分類A\n      項目1\n      項目2\n    分類B\n      項目3";

    fn parse(source: &str) -> MindmapDocument {
        parse_mindmap(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_mindmap(&first);
        let second = parse(&first_text);
        let second_text = emit_mindmap(&second);
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

    fn labels(document: &MindmapDocument) -> Vec<(usize, String)> {
        document
            .outline()
            .into_iter()
            .map(|entry| (entry.depth, entry.node.label))
            .collect()
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(
            labels(&document),
            vec![
                (0, "中心テーマ".to_string()),
                (1, "分類A".to_string()),
                (2, "項目1".to_string()),
                (2, "項目2".to_string()),
                (1, "分類B".to_string()),
                (2, "項目3".to_string()),
            ]
        );
        let root = document.root.as_ref().expect("the template has a root");
        assert_eq!(root.id.as_deref(), Some("root"));
        assert_eq!(root.shape, MindmapShape::Circle);
        assert_eq!(emit_mindmap(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_shape_icon_and_class_survives() {
        let source = "mindmap\n  root((円))\n    a[四角]\n    b(角丸)\n    c))爆発((\n    d)雲(\n    e{{六角形}}\n    素のテキスト\n    ::icon(fa fa-book)\n    :::urgent large\n    [ID なしの四角]\n    q[\"引用 ] あり\"]\n    m[\"`**太字**`\"]";
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        let shapes = parse(source)
            .outline()
            .into_iter()
            .map(|entry| entry.node.shape)
            .collect::<Vec<_>>();
        assert_eq!(
            shapes,
            vec![
                MindmapShape::Circle,
                MindmapShape::Square,
                MindmapShape::Rounded,
                MindmapShape::Bang,
                MindmapShape::Cloud,
                MindmapShape::Hexagon,
                MindmapShape::Default,
                MindmapShape::Square,
                MindmapShape::Square,
                MindmapShape::Square,
            ]
        );
        let plain = parse(source)
            .outline()
            .into_iter()
            .find(|entry| entry.node.label == "素のテキスト")
            .expect("the plain node is there");
        assert_eq!(plain.node.icon.as_deref(), Some("fa fa-book"));
        assert_eq!(plain.node.classes.as_deref(), Some("urgent large"));
        assert_renders(&emitted);
    }

    #[test]
    fn the_depth_is_read_relatively_and_rewritten_in_two_space_steps() {
        // `merman` hangs a node under the last one indented less than it, so
        // the uneven widths here make `c` a sibling of `b`, not its child.
        let source = "mindmap\nroot\n        a\n    b\n      c\n   d\n\tx";
        let document = parse(source);
        assert_eq!(
            labels(&document),
            vec![
                (0, "root".to_string()),
                (1, "a".to_string()),
                (1, "b".to_string()),
                (2, "c".to_string()),
                (1, "d".to_string()),
                (1, "x".to_string()),
            ]
        );
        assert_eq!(
            assert_stable(source),
            "mindmap\n  root\n    a\n    b\n      c\n    d\n    x"
        );
    }

    #[test]
    fn comments_and_the_preamble_keep_their_side_of_the_tree() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nmindmap\n  %% 先頭のメモ\n  root\n    a\n    %% 途中のメモ\n    b";
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
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 途中のメモ".to_string()),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nmindmap\n  %% 先頭のメモ\n  root\n    a\n    b\n    %% 途中のメモ"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for (source, expected) in [
            ("mindmap\n  a\n  b", "ルートが 2 つ"),
            ("mindmap root\n  child", "`mindmap` の行"),
            ("mindmap\n  root[複数行の\n  ラベル]", "複数行"),
            ("mindmap\n  root\n    a(-変な形-)", "形"),
            ("mindmap\n  root\n    a[x] %% メモ", "コメント"),
            ("mindmap\n  ::icon(fa fa-book)\n  root", "前にノード"),
            ("mindmap\n  :::c\n  root", "前にノード"),
            ("mindmap\n  root\n  ::icon(a)\n  ::icon(b)", "2 回"),
            ("mindmap\n  root\n  :::a\n  :::b", "2 回"),
            ("mindmap\n  root\n  ::icon(fa fa-book", "閉じ括弧"),
            ("mindmap\n  root\n  ::icon(fa) 余分", "読み飛ばし"),
            ("mindmap\n  root\n  :::", "クラス"),
            ("mindmap\n  root\n  ::icon()", "アイコン"),
        ] {
            let error = parse_mindmap(source)
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
    fn an_empty_fence_and_a_bare_header_are_a_map_without_a_root() {
        assert_eq!(parse(""), MindmapDocument::empty());
        assert_eq!(parse("mindmap"), MindmapDocument::empty());
        assert_eq!(emit_mindmap(&MindmapDocument::empty()), "mindmap");
    }

    #[test]
    fn other_diagram_types_are_not_mindmaps() {
        assert!(is_mindmap_source("mindmap\n  root"));
        assert!(is_mindmap_source("%% メモ\nmindmap\n  root"));
        assert!(!is_mindmap_source("mindmaps\n  root"));
        assert!(!is_mindmap_source("flowchart TD\n  A --> B"));
        assert!(parse_mindmap("flowchart TD\n  A --> B").is_err());
    }

    #[test]
    fn a_label_is_quoted_when_its_brackets_would_cut_it_short() {
        let mut node = MindmapNode::new("閉じ ] あり");
        assert!(!is_valid_mindmap_label(&MindmapNode {
            shape: MindmapShape::Square,
            ..node.clone()
        }));
        node.set_shape(MindmapShape::Square);
        assert!(node.quoted, "the label needs quotes inside `[ ]`");
        assert_eq!(emit_node_spec(&node), "[\"閉じ ] あり\"]");

        // A label that is fine as it is keeps the spelling it was read with.
        let mut plain = MindmapNode::new("普通");
        plain.set_shape(MindmapShape::Rounded);
        assert!(!plain.quoted);
        assert_eq!(emit_node_spec(&plain), "(普通)");

        // A rounded label starting with `-` would read as the `(-…-)` shape.
        let mut dash = MindmapNode::new("-負-");
        dash.set_shape(MindmapShape::Rounded);
        assert!(dash.quoted);
        assert!(is_valid_mindmap_label(&dash));

        // Going back to the default shape drops what only brackets carry.
        let mut shaped = parse("mindmap\n  root((中心))")
            .root
            .expect("there is a root");
        shaped.set_shape(MindmapShape::Default);
        assert_eq!(shaped.id, None);
        assert_eq!(emit_node_spec(&shaped), "中心");
    }

    #[test]
    fn labels_that_cannot_be_written_are_reported() {
        for (shape, label) in [
            (MindmapShape::Default, "かっこ (あり)"),
            (MindmapShape::Default, "メモ %% あり"),
            (MindmapShape::Default, "::icon(x)"),
            (MindmapShape::Default, ":::c"),
            (MindmapShape::Default, " 前の空白"),
            (MindmapShape::Default, ""),
            (MindmapShape::Square, "引用 \" と ] の両方"),
            (MindmapShape::Square, "改行\nあり"),
        ] {
            let node = MindmapNode {
                shape,
                ..MindmapNode::new(label)
            };
            assert!(
                mindmap_node_problem(&node).is_some(),
                "`{label}` in {shape:?} should be reported"
            );
        }
        for (shape, label) in [
            (MindmapShape::Default, "日本語のラベル"),
            (MindmapShape::Default, "閉じ ] だけ"),
            (MindmapShape::Square, "かっこ (あり)"),
            (MindmapShape::Circle, "50%"),
        ] {
            let node = MindmapNode {
                shape,
                ..MindmapNode::new(label)
            };
            assert_eq!(
                mindmap_node_problem(&node),
                None,
                "`{label}` in {shape:?} should be writable"
            );
            let document = MindmapDocument {
                root: Some(node.clone()),
                preserved_lines: Vec::new(),
            };
            let emitted = emit_mindmap(&document);
            assert_eq!(parse(&emitted), document, "{emitted}");
            assert_renders(&emitted);
        }

        let mut decorated = MindmapNode::new("a");
        decorated.icon = Some("fa)".to_string());
        assert!(mindmap_node_problem(&decorated).is_some());
        decorated.icon = None;
        decorated.classes = Some(" ".to_string());
        assert!(mindmap_node_problem(&decorated).is_some());
    }

    fn outline_of(source: &str) -> Vec<MindmapOutlineEntry> {
        parse(source).outline()
    }

    fn emit_outline(entries: Vec<MindmapOutlineEntry>) -> String {
        let document = MindmapDocument::from_outline(entries, Vec::new())
            .unwrap_or_else(|error| panic!("the outline should stay a tree: {error}"));
        emit_mindmap(&document)
    }

    const TREE: &str = "mindmap\n  r\n    a\n      a1\n      a2\n    b\n      b1\n    c";

    #[test]
    fn moving_a_node_takes_its_subtree_with_it() {
        let mut entries = outline_of(TREE);
        assert_eq!(outline_move(&mut entries, 1, true), Some(3));
        assert_eq!(
            emit_outline(entries.clone()),
            "mindmap\n  r\n    b\n      b1\n    a\n      a1\n      a2\n    c"
        );
        assert_eq!(outline_move(&mut entries, 3, false), Some(1));
        assert_eq!(emit_outline(entries.clone()), TREE);
        // The first child has nothing above it on its level, and a child
        // never leaves its parent by moving.
        assert_eq!(outline_move(&mut entries, 1, false), None);
        assert_eq!(outline_move(&mut entries, 3, true), None);
        assert_eq!(outline_move(&mut entries, 0, true), None);
        assert_eq!(emit_outline(entries), TREE);
    }

    #[test]
    fn indenting_and_outdenting_keep_the_rest_of_the_tree_in_place() {
        let mut entries = outline_of(TREE);
        assert!(outline_indent(&mut entries, 4), "b goes under a");
        assert_eq!(
            emit_outline(entries.clone()),
            "mindmap\n  r\n    a\n      a1\n      a2\n      b\n        b1\n    c"
        );
        assert_eq!(outline_outdent(&mut entries, 4), Some(4));
        assert_eq!(emit_outline(entries.clone()), TREE);

        // Outdenting a1 moves it after its parent's subtree, leaving a2 with a.
        assert_eq!(outline_outdent(&mut entries, 2), Some(3));
        assert_eq!(
            emit_outline(entries.clone()),
            "mindmap\n  r\n    a\n      a2\n    a1\n    b\n      b1\n    c"
        );

        // Neither the root nor its children can move a level up, and a first
        // child has no sibling to go under.
        let mut entries = outline_of(TREE);
        assert_eq!(outline_outdent(&mut entries, 0), None);
        assert_eq!(outline_outdent(&mut entries, 1), None);
        assert!(!outline_indent(&mut entries, 0));
        assert!(!outline_indent(&mut entries, 1));
        assert!(!outline_indent(&mut entries, 2));
        assert_eq!(emit_outline(entries), TREE);
    }

    #[test]
    fn adding_and_removing_work_on_whole_subtrees() {
        let mut entries = outline_of(TREE);
        let new = MindmapOutlineEntry {
            depth: 0,
            node: MindmapNode::new("新しい"),
        };
        assert_eq!(outline_add_child(&mut entries, 1, new.clone()), Some(4));
        assert_eq!(outline_add_sibling(&mut entries, 1, new.clone()), Some(5));
        assert_eq!(outline_add_sibling(&mut entries, 0, new), None);
        assert_eq!(
            emit_outline(entries.clone()),
            "mindmap\n  r\n    a\n      a1\n      a2\n      新しい\n    新しい\n    b\n      b1\n    c"
        );

        let mut entries = outline_of(TREE);
        let removed = outline_remove(&mut entries, 1).expect("a can be removed");
        assert_eq!(
            removed
                .iter()
                .map(|entry| entry.node.label.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "a1", "a2"]
        );
        assert_eq!(
            emit_outline(entries.clone()),
            "mindmap\n  r\n    b\n      b1\n    c"
        );
        assert!(outline_remove(&mut entries, 0).is_none(), "the root stays");
    }

    #[test]
    fn an_outline_that_is_not_a_tree_is_refused() {
        let entry = |depth: usize| MindmapOutlineEntry {
            depth,
            node: MindmapNode::new("x"),
        };
        assert!(MindmapDocument::from_outline(vec![entry(1)], Vec::new()).is_err());
        assert!(MindmapDocument::from_outline(vec![entry(0), entry(0)], Vec::new()).is_err());
        assert!(MindmapDocument::from_outline(vec![entry(0), entry(2)], Vec::new()).is_err());
        assert!(
            MindmapDocument::from_outline(Vec::new(), Vec::new())
                .is_ok_and(|document| document.root.is_none())
        );
    }

    #[test]
    fn a_deep_chain_does_not_recurse() {
        let mut source = String::from("mindmap\n");
        for depth in 0..2000 {
            source.push_str(&" ".repeat(depth + 1));
            source.push_str("n\n");
        }
        let document = scan_mindmap(&source).expect("a deep chain should scan");
        assert_eq!(document.outline().len(), 2000);
        let emitted = emit_mindmap(&document);
        assert_eq!(scan_mindmap(&emitted).expect("rescan"), document);
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "mindmap  \r\n  root  \r\n    a[四角]  \r\n";
        let document = parse(source);
        assert_eq!(
            labels(&document),
            vec![(0, "root".to_string()), (1, "四角".to_string())]
        );
        assert_eq!(emit_mindmap(&document), "mindmap\n  root\n    a[四角]");
    }

    /// Each edge as the labels of its two ends, the way `merman` connects them.
    fn merman_edges(source: &str) -> Vec<(String, String)> {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source:?}: {error}"))
            .unwrap_or_else(|| panic!("merman should find a diagram in {source:?}"));
        let RenderSemanticModel::Mindmap(model) = parsed.model else {
            panic!("{source:?} should be a mindmap");
        };
        let label_of = |id: &str| {
            model
                .nodes
                .iter()
                .find(|node| node.id == id)
                .map(|node| node.label.clone())
                .unwrap_or_default()
        };
        let mut edges = model
            .edges
            .iter()
            .map(|edge| (label_of(&edge.start), label_of(&edge.end)))
            .collect::<Vec<_>>();
        edges.sort();
        edges
    }

    fn model_edges(document: &MindmapDocument) -> Vec<(String, String)> {
        let entries = document.outline();
        let mut edges = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let parent = entries.get(outline_parent(&entries, index)?)?;
                Some((parent.node.label.clone(), entry.node.label.clone()))
            })
            .collect::<Vec<_>>();
        edges.sort();
        edges
    }

    /// `merman` counts every whitespace character as one column, tabs and
    /// ideographic spaces included, so mixed indentation has to hang each node
    /// under the same parent here and after the two-space rewrite.
    #[test]
    fn tabs_and_mixed_indentation_hang_nodes_where_merman_does() {
        for source in [
            "mindmap\n\troot\n\t\ta\n\t  b\n  \tc\n\t d\n\t\t\t\te",
            "mindmap\n root\n\t\t\ta\n  b\n   c\n\t\t\t\td",
            "mindmap\n\u{3000}root\n\u{3000}\u{3000}a\n\u{3000} b\n  c",
        ] {
            let document = parse(source);
            let expected = merman_edges(source);
            assert_eq!(model_edges(&document), expected, "{source:?}");
            let emitted = assert_stable(source);
            assert_eq!(merman_edges(&emitted), expected, "{emitted}");
        }
        for source in ["mindmap\n  root\n\ta", "mindmap\n\troot\n  a\n\tb"] {
            assert!(parse_mindmap(source).is_err(), "{source:?}");
            assert!(
                Engine::new()
                    .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
                    .is_err(),
                "merman should refuse {source:?} too"
            );
        }
    }

    #[test]
    fn brackets_and_hashes_inside_labels_do_not_change_the_shape() {
        for (source, label, shape) in [
            ("mindmap\n  a[\"a (b)\"]", "a (b)", MindmapShape::Square),
            ("mindmap\n  a(\"x (y)\")", "x (y)", MindmapShape::Rounded),
            ("mindmap\n  a((\"x))\"))", "x))", MindmapShape::Circle),
            ("mindmap\n  (\"(b)\")", "(b)", MindmapShape::Rounded),
            ("mindmap\n  a{{\"}} x\"}}", "}} x", MindmapShape::Hexagon),
            ("mindmap\n  x))\"((y\"((", "((y", MindmapShape::Bang),
            ("mindmap\n  x)\"(y\"(", "(y", MindmapShape::Cloud),
            ("mindmap\n  C# 言語", "C# 言語", MindmapShape::Default),
            (
                "mindmap\n  b[\"# 見出し\"]",
                "# 見出し",
                MindmapShape::Square,
            ),
        ] {
            let document = parse(source);
            let root = document.root.as_ref().expect("there is a root");
            assert_eq!(
                (root.label.as_str(), root.shape),
                (label, shape),
                "{source}"
            );
            assert_eq!(emit_mindmap(&document), source);
        }
    }
}
