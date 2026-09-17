//! A round-trippable editing model for Mermaid flowcharts.
//!
//! Mermaid has no reverse transform: `merman` parses source into a render
//! model, and nothing turns a render model back into source. A structured
//! editor needs both directions, so this module pairs [`parse_flowchart`] —
//! which borrows `merman`'s parser and copies the parts of its flowchart model
//! that map onto source constructs — with [`emit_flowchart`], which writes those
//! parts back out.
//!
//! Two properties matter more than fidelity to the original spelling:
//!
//! - [`emit_flowchart`] is idempotent from its second application. Parsing its
//!   own output and emitting again produces the identical string, so opening
//!   and confirming the editor twice does not keep rewriting the fence.
//! - Everything `merman`'s model does not carry — comments, `%%{init}%%`
//!   directives, `classDef` / `style` / `linkStyle` / `click`, accessibility
//!   statements — survives verbatim in [`FlowchartDocument::preserved_lines`],
//!   because dropping a user's styling on an unrelated edit would be worse than
//!   any amount of re-spelling.
//!
//! Known losses, all of which concern constructs the editor does not expose:
//! user defined edge ids (`A e1@--> B`) and therefore any `class` statement that
//! names an edge rather than a node or subgraph; per-node `style` is kept as a
//! preserved statement but is not attached to the node it styles.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::diagrams::flowchart::FlowchartV2Model;
use merman::{Engine, ParseOptions};

/// The diagram keyword the source opened with. `flowchart` and `graph` select
/// the same renderer, but rewriting one as the other would be a gratuitous
/// change to the user's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowchartKeyword {
    Flowchart,
    Graph,
}

impl FlowchartKeyword {
    fn as_str(self) -> &'static str {
        match self {
            Self::Flowchart => "flowchart",
            Self::Graph => "graph",
        }
    }
}

/// The layout direction of a flowchart or of one of its subgraphs.
///
/// `TD` is absent because `merman` normalizes it to `TB` while parsing, so a
/// document that came back as `TB` can no longer tell which of the two the user
/// typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowDirection {
    TopToBottom,
    BottomToTop,
    LeftToRight,
    RightToLeft,
}

impl FlowDirection {
    pub const ALL: [FlowDirection; 4] = [
        FlowDirection::TopToBottom,
        FlowDirection::LeftToRight,
        FlowDirection::BottomToTop,
        FlowDirection::RightToLeft,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::TopToBottom => "TB",
            Self::BottomToTop => "BT",
            Self::LeftToRight => "LR",
            Self::RightToLeft => "RL",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TopToBottom => "上から下 (TB / TD)",
            Self::BottomToTop => "下から上 (BT)",
            Self::LeftToRight => "左から右 (LR)",
            Self::RightToLeft => "右から左 (RL)",
        }
    }

    fn from_model(direction: Option<&str>) -> Self {
        match direction.map(str::trim) {
            Some("BT") => Self::BottomToTop,
            Some("LR") => Self::LeftToRight,
            Some("RL") => Self::RightToLeft,
            _ => Self::TopToBottom,
        }
    }
}

/// The shape of a flowchart node.
///
/// The named variants are the shapes Mermaid spells with brackets. Everything
/// else goes through the Mermaid 11 shape attribute `A@{ shape: name }`, which
/// only accepts lowercase names without underscores — which is exactly why the
/// bracket-only shapes (`lean_left`, `inv_trapezoid`, …) have variants here
/// rather than being folded into [`NodeShape::Named`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeShape {
    Rectangle,
    Rounded,
    Stadium,
    Subroutine,
    Cylinder,
    Circle,
    DoubleCircle,
    Ellipse,
    Asymmetric,
    Rhombus,
    Hexagon,
    ParallelogramRight,
    ParallelogramLeft,
    TrapezoidUp,
    TrapezoidDown,
    Named(String),
}

impl NodeShape {
    /// The shapes offered in the editor's shape menu, in menu order.
    pub const PRESETS: [NodeShape; 10] = [
        NodeShape::Rectangle,
        NodeShape::Rounded,
        NodeShape::Stadium,
        NodeShape::Subroutine,
        NodeShape::Cylinder,
        NodeShape::Circle,
        NodeShape::Asymmetric,
        NodeShape::Rhombus,
        NodeShape::Hexagon,
        NodeShape::ParallelogramRight,
    ];

    pub fn label(&self) -> String {
        match self {
            Self::Rectangle => "長方形 [処理]".into(),
            Self::Rounded => "角丸 (処理)".into(),
            Self::Stadium => "端が丸い ([開始])".into(),
            Self::Subroutine => "サブルーチン [[処理]]".into(),
            Self::Cylinder => "データベース [(保存)]".into(),
            Self::Circle => "円 ((点))".into(),
            Self::DoubleCircle => "二重丸 (((終端)))".into(),
            Self::Ellipse => "楕円 (-楕円-)".into(),
            Self::Asymmetric => "旗 >注記]".into(),
            Self::Rhombus => "ひし形 {条件}".into(),
            Self::Hexagon => "六角形 {{準備}}".into(),
            Self::ParallelogramRight => "平行四辺形 [/入力/]".into(),
            Self::ParallelogramLeft => "平行四辺形 (逆) [\\出力\\]".into(),
            Self::TrapezoidUp => "台形 [/手操作\\]".into(),
            Self::TrapezoidDown => "台形 (逆) [\\手入力/]".into(),
            Self::Named(name) => format!("その他: {name}"),
        }
    }

    /// The opening and closing delimiters for the bracket spellings.
    fn delimiters(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Rectangle => Some(("[", "]")),
            Self::Rounded => Some(("(", ")")),
            Self::Stadium => Some(("([", "])")),
            Self::Subroutine => Some(("[[", "]]")),
            Self::Cylinder => Some(("[(", ")]")),
            Self::Circle => Some(("((", "))")),
            Self::DoubleCircle => Some(("(((", ")))")),
            Self::Ellipse => Some(("(-", "-)")),
            Self::Asymmetric => Some((">", "]")),
            Self::Rhombus => Some(("{", "}")),
            Self::Hexagon => Some(("{{", "}}")),
            Self::ParallelogramRight => Some(("[/", "/]")),
            Self::ParallelogramLeft => Some(("[\\", "\\]")),
            Self::TrapezoidUp => Some(("[/", "\\]")),
            Self::TrapezoidDown => Some(("[\\", "/]")),
            Self::Named(_) => None,
        }
    }

    fn from_model(layout_shape: Option<&str>) -> Self {
        match layout_shape.unwrap_or("squareRect") {
            "squareRect" => Self::Rectangle,
            "roundedRect" => Self::Rounded,
            "stadium" => Self::Stadium,
            "subroutine" => Self::Subroutine,
            "cylinder" => Self::Cylinder,
            "circle" => Self::Circle,
            "doublecircle" => Self::DoubleCircle,
            "ellipse" => Self::Ellipse,
            "odd" => Self::Asymmetric,
            "diamond" => Self::Rhombus,
            "hexagon" => Self::Hexagon,
            "lean_right" => Self::ParallelogramRight,
            "lean_left" => Self::ParallelogramLeft,
            "trapezoid" => Self::TrapezoidUp,
            "inv_trapezoid" => Self::TrapezoidDown,
            other => Self::Named(other.to_string()),
        }
    }
}

/// The arrow head of an edge, including the `double_` forms that put a head on
/// both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowKind {
    None,
    Arrow,
    Cross,
    Circle,
    DoubleArrow,
    DoubleCross,
    DoubleCircle,
}

impl ArrowKind {
    fn head(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Arrow | Self::DoubleArrow => ">",
            Self::Cross | Self::DoubleCross => "x",
            Self::Circle | Self::DoubleCircle => "o",
        }
    }

    fn tail(self) -> &'static str {
        match self {
            Self::DoubleArrow => "<",
            Self::DoubleCross => "x",
            Self::DoubleCircle => "o",
            _ => "",
        }
    }

    fn from_model(edge_type: Option<&str>) -> Self {
        match edge_type.unwrap_or("arrow_point") {
            "arrow_open" => Self::None,
            "arrow_cross" => Self::Cross,
            "arrow_circle" => Self::Circle,
            "double_arrow_point" => Self::DoubleArrow,
            "double_arrow_cross" => Self::DoubleCross,
            "double_arrow_circle" => Self::DoubleCircle,
            _ => Self::Arrow,
        }
    }
}

/// The line an edge is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeKind {
    Normal,
    Thick,
    Dotted,
    Invisible,
}

impl StrokeKind {
    fn from_model(stroke: Option<&str>) -> Self {
        match stroke.unwrap_or("normal") {
            "thick" => Self::Thick,
            "dotted" => Self::Dotted,
            "invisible" => Self::Invisible,
            _ => Self::Normal,
        }
    }
}

/// One entry of the editor's arrow menu: a spelling the user picks by sight.
pub struct EdgeStylePreset {
    pub operator: &'static str,
    pub label: &'static str,
    pub arrow: ArrowKind,
    pub stroke: StrokeKind,
}

pub const EDGE_STYLE_PRESETS: [EdgeStylePreset; 8] = [
    EdgeStylePreset {
        operator: "-->",
        label: "矢印",
        arrow: ArrowKind::Arrow,
        stroke: StrokeKind::Normal,
    },
    EdgeStylePreset {
        operator: "---",
        label: "線のみ",
        arrow: ArrowKind::None,
        stroke: StrokeKind::Normal,
    },
    EdgeStylePreset {
        operator: "-.->",
        label: "点線の矢印",
        arrow: ArrowKind::Arrow,
        stroke: StrokeKind::Dotted,
    },
    EdgeStylePreset {
        operator: "==>",
        label: "太い矢印",
        arrow: ArrowKind::Arrow,
        stroke: StrokeKind::Thick,
    },
    EdgeStylePreset {
        operator: "<-->",
        label: "両向き矢印",
        arrow: ArrowKind::DoubleArrow,
        stroke: StrokeKind::Normal,
    },
    EdgeStylePreset {
        operator: "--x",
        label: "×印",
        arrow: ArrowKind::Cross,
        stroke: StrokeKind::Normal,
    },
    EdgeStylePreset {
        operator: "--o",
        label: "○印",
        arrow: ArrowKind::Circle,
        stroke: StrokeKind::Normal,
    },
    EdgeStylePreset {
        operator: "~~~",
        label: "非表示 (位置合わせ)",
        arrow: ArrowKind::None,
        stroke: StrokeKind::Invisible,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowchartNode {
    pub id: String,
    pub label: String,
    /// Whether the label was written as a Mermaid markdown string
    /// (`` A["`**bold**`"] ``). The editor does not offer this, but rewriting
    /// such a label as plain text would silently drop its formatting.
    pub markdown_label: bool,
    pub shape: NodeShape,
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowchartEdge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    pub markdown_label: bool,
    pub arrow: ArrowKind,
    pub stroke: StrokeKind,
    /// How many layout ranks the edge spans; `A --> B` is 1 and each extra dash
    /// adds one. Not editable, but rewriting every long edge as a short one
    /// would rearrange the picture.
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowchartSubgraph {
    pub id: String,
    pub title: String,
    pub direction: Option<FlowDirection>,
    pub node_ids: Vec<String>,
    pub classes: Vec<String>,
    /// Index into [`FlowchartDocument::subgraphs`] of the subgraph this one is
    /// nested in.
    pub parent: Option<usize>,
}

/// Where a line that the model does not carry is written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreservedPosition {
    /// Before the `flowchart` line. Frontmatter and `%%{init}%%` directives
    /// have to precede the diagram keyword.
    Preamble,
    /// Directly after the `flowchart` line, for lines that came before the
    /// first node or edge.
    Top,
    /// After everything else.
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedLine {
    pub position: PreservedPosition,
    /// Verbatim source, including its original indentation. A statement that
    /// spans lines (a `%%{init}%%` block, an `accDescr { … }` block) is held as
    /// one entry with embedded newlines.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowchartDocument {
    pub keyword: FlowchartKeyword,
    pub direction: FlowDirection,
    pub nodes: Vec<FlowchartNode>,
    pub edges: Vec<FlowchartEdge>,
    pub subgraphs: Vec<FlowchartSubgraph>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl FlowchartDocument {
    /// A blank flowchart, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: FlowchartKeyword::Flowchart,
            direction: FlowDirection::TopToBottom,
            nodes: Vec::new(),
            edges: Vec::new(),
            subgraphs: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    pub fn node_index(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == id)
    }

    /// An id that no node and no subgraph is using, based on `prefix`.
    pub fn unused_id(&self, prefix: &str) -> String {
        let taken = |candidate: &str| {
            self.nodes.iter().any(|node| node.id == candidate)
                || self.subgraphs.iter().any(|group| group.id == candidate)
        };
        if !taken(prefix) {
            return prefix.to_string();
        }
        for suffix in 1..usize::MAX {
            let candidate = format!("{prefix}{suffix}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        prefix.to_string()
    }
}

/// Whether `source` is a flowchart, i.e. whether the structured editor can open
/// it at all. Cheap enough to call before parsing.
pub fn is_flowchart_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| header_keyword(line).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_flowchart(source: &str) -> Result<FlowchartDocument> {
    let scan = scan_source(source)?;

    let model = if source.trim().is_empty() {
        None
    } else {
        let engine = Engine::new();
        let parsed = engine
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .map_err(|error| anyhow!("{error}"))?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        match parsed.model {
            RenderSemanticModel::Flowchart(model) => Some(model),
            _ => bail!("この図種は構造化編集に対応していません (フローチャートのみ)。"),
        }
    };

    let Some(model) = model else {
        return Ok(FlowchartDocument {
            keyword: scan.keyword,
            direction: FlowDirection::TopToBottom,
            nodes: Vec::new(),
            edges: Vec::new(),
            subgraphs: Vec::new(),
            preserved_lines: scan.preserved,
        });
    };

    let document = document_from_model(model, scan.keyword, scan.preserved);
    if let Some(problem) = unsupported_construct(&scan.statements, &document) {
        bail!("{problem}");
    }
    Ok(document)
}

fn document_from_model(
    model: FlowchartV2Model,
    keyword: FlowchartKeyword,
    preserved: Vec<PreservedLine>,
) -> FlowchartDocument {
    let direction = FlowDirection::from_model(model.direction.as_deref());
    let clicked = click_targets(&preserved);

    let nodes = model
        .nodes
        .into_iter()
        .map(|node| FlowchartNode {
            label: node.label.clone().unwrap_or_else(|| node.id.clone()),
            markdown_label: node.label_type.as_deref() == Some("markdown"),
            shape: NodeShape::from_model(node.layout_shape.as_deref()),
            classes: without_click_classes(node.classes, &node.id, &clicked),
            id: node.id,
        })
        .collect::<Vec<_>>();

    let edges = model
        .edges
        .into_iter()
        .map(|edge| FlowchartEdge {
            from: edge.from,
            to: edge.to,
            markdown_label: edge.label_type.as_deref() == Some("markdown"),
            label: edge.label,
            arrow: ArrowKind::from_model(edge.edge_type.as_deref()),
            stroke: StrokeKind::from_model(edge.stroke.as_deref()),
            length: edge.length.max(1),
        })
        .collect::<Vec<_>>();

    // A nested subgraph stays in its parent's member list under its own id
    // (the parser only drops members that a *deeper* block already claimed), so
    // the nesting is read back from the members rather than from the source.
    let parents = model
        .subgraphs
        .iter()
        .map(|group| {
            model
                .subgraphs
                .iter()
                .position(|candidate| candidate.nodes.contains(&group.id))
        })
        .collect::<Vec<_>>();

    let subgraphs = model
        .subgraphs
        .into_iter()
        .enumerate()
        .map(|(index, group)| FlowchartSubgraph {
            classes: without_click_classes(group.classes, &group.id, &clicked),
            id: group.id,
            title: group.title,
            direction: group
                .dir
                .as_deref()
                .map(|dir| FlowDirection::from_model(Some(dir))),
            node_ids: group.nodes,
            parent: parents.get(index).copied().flatten(),
        })
        .collect::<Vec<_>>();

    FlowchartDocument {
        keyword,
        direction,
        nodes,
        edges,
        subgraphs,
        preserved_lines: preserved,
    }
}

/// The ids named by the preserved `click` statements.
fn click_targets(preserved: &[PreservedLine]) -> Vec<&str> {
    preserved
        .iter()
        .filter_map(|line| {
            let rest = line.text.trim().strip_prefix("click")?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            rest.split_whitespace().next()
        })
        .collect()
}

/// `merman` marks the target of a `click` statement with a synthetic
/// `clickable` class. The statement itself is preserved verbatim and will mark
/// the target again on the next parse, so re-emitting that class as a `class`
/// statement of its own would add one more `clickable` on every visit to the
/// editor.
fn without_click_classes(mut classes: Vec<String>, id: &str, clicked: &[&str]) -> Vec<String> {
    for _ in clicked.iter().filter(|target| **target == id) {
        if let Some(position) = classes.iter().position(|class| class == "clickable") {
            classes.remove(position);
        }
    }
    classes
}

const INDENT: &str = "    ";

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_flowchart(document: &FlowchartDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    let _ = writeln!(
        out,
        "{} {}",
        document.keyword.as_str(),
        document.direction.keyword()
    );

    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    for node in &document.nodes {
        let _ = writeln!(out, "{INDENT}{}", node_declaration(node));
    }

    for edge in &document.edges {
        let _ = writeln!(out, "{INDENT}{}", edge_statement(edge));
    }

    let children = subgraph_children(document);
    for index in children.get(&None).into_iter().flatten() {
        emit_subgraph(document, &children, *index, 1, &mut out);
    }

    for (id, classes) in class_assignments(document) {
        let _ = writeln!(out, "{INDENT}class {id} {}", classes.join(","));
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
    document: &FlowchartDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn class_assignments(document: &FlowchartDocument) -> Vec<(&str, &Vec<String>)> {
    let nodes = document
        .nodes
        .iter()
        .filter(|node| !node.classes.is_empty())
        .map(|node| (node.id.as_str(), &node.classes));
    let subgraphs = document
        .subgraphs
        .iter()
        .filter(|group| !group.classes.is_empty())
        .map(|group| (group.id.as_str(), &group.classes));
    nodes.chain(subgraphs).collect()
}

/// Maps each subgraph to the subgraphs directly inside it, keyed by
/// `Some(index)`, with the top level under `None`.
fn subgraph_children(document: &FlowchartDocument) -> HashMap<Option<usize>, Vec<usize>> {
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (index, group) in document.subgraphs.iter().enumerate() {
        // Blocks are listed in the order they close, so a parent always comes
        // after its children. Anything else would make the recursion below
        // either loop or lose the block, and is treated as top level.
        let parent = group.parent.filter(|parent| *parent > index);
        children.entry(parent).or_default().push(index);
    }
    children
}

fn emit_subgraph(
    document: &FlowchartDocument,
    children: &HashMap<Option<usize>, Vec<usize>>,
    index: usize,
    depth: usize,
    out: &mut String,
) {
    let Some(group) = document.subgraphs.get(index) else {
        return;
    };
    let indent = INDENT.repeat(depth);
    let inner = INDENT.repeat(depth + 1);

    let _ = writeln!(
        out,
        "{indent}subgraph {} [{}]",
        group.id,
        quoted_label(&group.title, false)
    );
    if let Some(direction) = group.direction {
        let _ = writeln!(out, "{inner}direction {}", direction.keyword());
    }
    for child in children.get(&Some(index)).into_iter().flatten() {
        emit_subgraph(document, children, *child, depth + 1, out);
    }
    for node_id in &group.node_ids {
        // A nested block is a member of its parent under its own id; writing
        // that id as a bare statement as well would declare a node beside it.
        if document.subgraphs.iter().any(|group| group.id == *node_id) {
            continue;
        }
        let _ = writeln!(out, "{inner}{node_id}");
    }
    let _ = writeln!(out, "{indent}end");
}

fn node_declaration(node: &FlowchartNode) -> String {
    match node.shape.delimiters() {
        Some((open, close)) => {
            if node.shape == NodeShape::Rectangle && !node.markdown_label && node.label == node.id {
                return node.id.clone();
            }
            format!(
                "{}{open}{}{close}",
                node.id,
                label_text(&node.label, node.markdown_label)
            )
        }
        None => {
            let NodeShape::Named(name) = &node.shape else {
                return node.id.clone();
            };
            format!(
                "{}@{{ shape: {name}, label: \"{}\" }}",
                node.id,
                escape_quoted(&node.label)
            )
        }
    }
}

fn edge_statement(edge: &FlowchartEdge) -> String {
    let operator = edge_operator(edge.arrow, edge.stroke, edge.length);
    match &edge.label {
        Some(label) => format!(
            "{} {operator}|{}| {}",
            edge.from,
            label_text(label, edge.markdown_label),
            edge.to
        ),
        None => format!("{} {operator} {}", edge.from, edge.to),
    }
}

/// The inverse of `merman`'s `destruct_start_link` / `destruct_end_link`: the
/// link operator that parses back to exactly this arrow, stroke and length.
fn edge_operator(arrow: ArrowKind, stroke: StrokeKind, length: usize) -> String {
    // `merman` clamps parsed lengths to 10 and derives them from a line of at
    // least one character, so anything outside that never round-trips.
    let length = length.clamp(1, 10);
    let head = arrow.head();
    let tail = arrow.tail();
    let body = match stroke {
        StrokeKind::Dotted => format!("-{}-", ".".repeat(length)),
        StrokeKind::Normal | StrokeKind::Thick | StrokeKind::Invisible => {
            let character = match stroke {
                StrokeKind::Thick => "=",
                StrokeKind::Invisible => "~",
                _ => "-",
            };
            // Without a head the operator's last character is part of the line
            // itself, which is one more character for the same length.
            let count = if head.is_empty() {
                length + 2
            } else {
                length + 1
            };
            character.repeat(count)
        }
    };
    format!("{tail}{body}{head}")
}

/// A label as it appears between a shape's delimiters or between a pair of
/// `|`: bare when Mermaid's unquoted text rules allow it, quoted otherwise.
fn label_text(label: &str, markdown: bool) -> String {
    if markdown {
        return format!("\"`{}`\"", escape_quoted(label));
    }
    if needs_quoting(label) {
        format!("\"{}\"", escape_quoted(label))
    } else {
        label.to_string()
    }
}

fn quoted_label(label: &str, markdown: bool) -> String {
    label_text(label, markdown)
}

/// Whether Mermaid would misread `label` if it were written without quotes.
///
/// The parser rejects the structural characters outright, stops an unquoted
/// label at `;` or a newline, and trims it, so anything that would come back
/// different has to be quoted. `/` and `\` close the parallelogram and
/// trapezoid shapes, and are quoted everywhere so the check does not depend on
/// which shape the label ends up in.
fn needs_quoting(label: &str) -> bool {
    label.is_empty()
        || label.trim() != label
        || label.chars().any(|character| {
            matches!(
                character,
                '"' | '\''
                    | '['
                    | ']'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '|'
                    | ';'
                    | '`'
                    | '/'
                    | '\\'
                    | '\n'
                    | '\r'
            )
        })
}

/// Mermaid has no escape for a quote inside a quoted label; it decodes the
/// `#…;` entity shorthand instead. `|` gets the same treatment because the
/// lexer's edge label scan stops at the first `|` whether or not it is inside
/// quotes, and so does `\`, which would otherwise escape the quote that closes
/// the label. A newline cannot survive inside a statement at all, so it becomes
/// the line break Mermaid draws.
fn escape_quoted(label: &str) -> String {
    label
        .replace('"', "#quot;")
        .replace('|', "#124;")
        .replace('\\', "#92;")
        .replace("\r\n", "<br/>")
        .replace(['\n', '\r'], "<br/>")
}

/// Whether `id` can be written as a bare Mermaid node id.
pub fn is_valid_node_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !is_reserved_id(id)
}

/// Keywords that start a statement of their own. Mermaid's best known trap is a
/// node called `end`, which closes the enclosing subgraph instead.
fn is_reserved_id(id: &str) -> bool {
    const RESERVED: [&str; 12] = [
        "end",
        "graph",
        "flowchart",
        "subgraph",
        "direction",
        "class",
        "classDef",
        "style",
        "linkStyle",
        "click",
        "callback",
        "default",
    ];
    RESERVED.iter().any(|reserved| reserved == &id)
}

/// What the line scanner recovers from the source text that the parsed model
/// cannot express.
struct SourceScan {
    keyword: FlowchartKeyword,
    preserved: Vec<PreservedLine>,
    /// The lines the scanner handed to the parser, i.e. everything that is
    /// neither a header nor preserved. Only the model speaks for these, so they
    /// are where a construct the model drops has to be caught.
    statements: Vec<String>,
}

fn first_statement_line(source: &str) -> Option<&str> {
    let mut lines = source.lines().map(str::trim).peekable();
    let mut in_frontmatter = false;
    let mut seen_content = false;
    let mut directive_depth = 0usize;

    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if directive_depth > 0 {
            if line.contains("}%%") {
                directive_depth = 0;
            }
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if !seen_content && line == "---" {
            seen_content = true;
            in_frontmatter = true;
            continue;
        }
        seen_content = true;
        if line.starts_with("%%") {
            if line.starts_with("%%{") && !line.contains("}%%") {
                directive_depth = 1;
            }
            continue;
        }
        return Some(line);
    }
    None
}

fn header_keyword(line: &str) -> Option<FlowchartKeyword> {
    for (keyword, value) in [
        ("flowchart", FlowchartKeyword::Flowchart),
        ("graph", FlowchartKeyword::Graph),
    ] {
        let Some(rest) = line.strip_prefix(keyword) else {
            continue;
        };
        if rest.is_empty()
            || rest.starts_with(|character: char| character.is_whitespace() || character == ';')
        {
            return Some(value);
        }
    }
    None
}

/// Statements that `merman`'s flowchart model does not carry, and that are
/// therefore written back verbatim.
///
/// `class` is deliberately absent: the model does carry class membership, so
/// keeping the statement as well as re-emitting it from the model would apply
/// it twice.
fn preserved_keyword(line: &str) -> bool {
    const KEYWORDS: [&str; 7] = [
        "classDef",
        "style",
        "linkStyle",
        "click",
        "callback",
        "accTitle",
        "accDescr",
    ];
    KEYWORDS.iter().any(|keyword| {
        line.strip_prefix(keyword).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with(|character: char| {
                    character.is_whitespace() || character == ':' || character == '{'
                })
        })
    })
}

fn scan_source(source: &str) -> Result<SourceScan> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut preserved = Vec::new();
    let mut index = 0;
    let mut seen_content = false;
    let mut keyword = None;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        if !seen_content && trimmed == "---" {
            seen_content = true;
            let start = index;
            index += 1;
            while index < lines.len() && lines[index].trim() != "---" {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(&lines[start..index]),
            });
            continue;
        }
        seen_content = true;
        if trimmed.starts_with("%%") {
            let start = index;
            index += 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while index < lines.len() && !lines[index].contains("}%%") {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(&lines[start..index]),
            });
            continue;
        }
        let Some(found) = header_keyword(trimmed) else {
            bail!("この図種は構造化編集に対応していません (フローチャートのみ)。");
        };
        keyword = Some(found);
        index += 1;
        break;
    }

    let Some(keyword) = keyword else {
        // An empty fence: the editor opens on a blank flowchart, and whatever
        // comments the user left keep their place.
        return Ok(SourceScan {
            keyword: FlowchartKeyword::Flowchart,
            preserved,
            statements: Vec::new(),
        });
    };

    let mut statements = Vec::new();
    let mut position = PreservedPosition::Top;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        index += 1;
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            let start = index - 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while index < lines.len() && !lines[index].contains("}%%") {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position,
                text: join_lines(&lines[start..index]),
            });
            continue;
        }
        if preserved_keyword(trimmed) {
            let start = index - 1;
            if trimmed.ends_with('{') {
                while index < lines.len() && lines[index].trim() != "}" {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position,
                text: join_lines(&lines[start..index]),
            });
            continue;
        }

        statements.push(trimmed.to_string());
        position = PreservedPosition::Bottom;
    }

    Ok(SourceScan {
        keyword,
        preserved,
        statements,
    })
}

fn join_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why `statements` cannot be edited as a form, if there is a reason.
///
/// A statement the model carries only in part is worse than one it refuses:
/// the editor would write the diagram back without the part it dropped. Rather
/// than silently rewriting such a fence, the editor declines to open it and
/// says which spelling is in the way.
fn unsupported_construct(statements: &[String], document: &FlowchartDocument) -> Option<String> {
    for statement in statements {
        let outside = outside_labels(statement);
        if outside.contains("%%") {
            return Some(format!(
                "文の末尾のコメントは構造化編集で失われます ({statement})。コメントを独立した行に移すか、テキストのまま編集してください。"
            ));
        }
        let Some(at) = outside.find('@') else {
            continue;
        };
        let target = outside
            .get(..at)
            .unwrap_or_default()
            .split_whitespace()
            .next_back()
            .unwrap_or_default();
        // `id@{ … }` on a node is the Mermaid 11 shape spelling, which the
        // model carries. On anything else — an edge — it is metadata that only
        // the edge id it names can address, and the id itself does not survive.
        if outside.get(at + 1..).is_some_and(|rest| {
            rest.trim_start().starts_with('{') && document.node_index(target).is_some()
        }) {
            continue;
        }
        return Some(format!(
            "エッジ ID (`{target}@`) を使った図は構造化編集に対応していません ({statement})。テキストのまま編集してください。"
        ));
    }
    None
}

/// `statement` with everything the parser reads as a label — text in quotes,
/// between a shape's brackets or between a pair of `|` — blanked out, so that a
/// `@` or a `%%` the user wrote in a caption is not mistaken for syntax.
fn outside_labels(statement: &str) -> String {
    let mut out = String::with_capacity(statement.len());
    let mut quoted = false;
    let mut piped = false;
    let mut depth = 0usize;
    let mut previous = ' ';
    for character in statement.chars() {
        let structural = !quoted && !piped && depth == 0;
        match character {
            '"' => quoted = !quoted,
            '|' if !quoted && depth == 0 => piped = !piped,
            // `@{` opens a shape attribute rather than a label.
            '[' | '(' | '{' if !quoted && !piped && previous != '@' => depth += 1,
            ']' | ')' | '}' if !quoted && !piped && depth > 0 => depth -= 1,
            _ => {}
        }
        out.push(if structural && character != '"' {
            character
        } else {
            ' '
        });
        previous = character;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    fn round_trip(source: &str) -> (FlowchartDocument, String, FlowchartDocument, String) {
        let first = parse_flowchart(source).expect("source should parse");
        let first_text = emit_flowchart(&first);
        let second = parse_flowchart(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_flowchart(&second);
        (first, first_text, second, second_text)
    }

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let (first, first_text, second, second_text) = round_trip(source);
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        assert_eq!(
            first.nodes, second.nodes,
            "nodes changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.edges, second.edges,
            "edges changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.subgraphs, second.subgraphs,
            "subgraphs changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.direction, second.direction,
            "direction changed through a round trip of:\n{first_text}"
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

    const EXAMPLES: [&str; 12] = [
        "flowchart TD\n    start[開始] --> condition{条件}\n    condition -->|はい| work[処理]\n    condition -->|いいえ| finish[終了]\n    work --> finish",
        "graph LR\n  A --> B",
        "flowchart TD\n  A([開始]) --> B[[サブルーチン]] --> C[(保存)] --> D((丸))",
        "flowchart LR\n  A>旗] --> B{{六角形}} --> C[/入力/] --> D[\\出力\\]",
        "flowchart TD\n  A[/手操作\\] --> B[\\手入力/] --> C(((終端)))",
        "flowchart BT\n  A -.-> B ==> C --- D --x E --o F",
        "flowchart RL\n  A <--> B\n  C ~~~ D\n  E -..-> F\n  G ---> H",
        "flowchart TD\n  subgraph one [グループ 1]\n    direction LR\n    A --> B\n  end\n  subgraph two [グループ 2]\n    C --> D\n  end\n  B --> C",
        "flowchart TD\n  %% コメント\n  A[\"角 [かっこ] 入り\"] --> B[\"引用 #quot;付き#quot;\"]\n  classDef warn fill:#f96\n  class A warn",
        "flowchart TD\n  A@{ shape: hourglass, label: \"砂時計\" } --> B@{ shape: bolt, label: \"稲妻\" }",
        "flowchart TD\n  A[\"`**太字**のラベル`\"] --> B\n  A -->|\"`*斜体*`\"| C",
        "flowchart TD\n  subgraph outer [外]\n    subgraph inner [内]\n      A --> B\n    end\n    C --> A\n  end\n  D --> C",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn flowchart_template_round_trips() {
        let template = "flowchart TD\n    %% [] は処理、{} は分岐。|ラベル| で分岐に注釈を付ける\n    start[開始] --> condition{条件}\n    condition -->|はい| work[処理]\n    condition -->|いいえ| finish[終了]\n    work --> finish";
        let emitted = assert_stable(template);
        assert!(
            emitted.contains("%% [] は処理"),
            "the template comment should survive:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn every_arrow_and_stroke_combination_round_trips() {
        let arrows = [
            ArrowKind::None,
            ArrowKind::Arrow,
            ArrowKind::Cross,
            ArrowKind::Circle,
            ArrowKind::DoubleArrow,
            ArrowKind::DoubleCross,
            ArrowKind::DoubleCircle,
        ];
        let strokes = [
            StrokeKind::Normal,
            StrokeKind::Thick,
            StrokeKind::Dotted,
            StrokeKind::Invisible,
        ];
        for arrow in arrows {
            for stroke in strokes {
                if stroke == StrokeKind::Invisible && arrow != ArrowKind::None {
                    continue;
                }
                for length in 1..=3 {
                    let operator = edge_operator(arrow, stroke, length);
                    let source = format!("flowchart TD\n  A {operator} B");
                    let document = parse_flowchart(&source)
                        .unwrap_or_else(|error| panic!("`{operator}` should parse: {error}"));
                    let edge = document
                        .edges
                        .first()
                        .unwrap_or_else(|| panic!("`{operator}` should produce an edge"));
                    assert_eq!(edge.arrow, arrow, "arrow of `{operator}`");
                    assert_eq!(edge.stroke, stroke, "stroke of `{operator}`");
                    assert_eq!(edge.length, length, "length of `{operator}`");
                    assert_eq!(
                        edge_operator(edge.arrow, edge.stroke, edge.length),
                        operator
                    );
                }
            }
        }
    }

    #[test]
    fn every_bracket_shape_round_trips() {
        let shapes = [
            NodeShape::Rectangle,
            NodeShape::Rounded,
            NodeShape::Stadium,
            NodeShape::Subroutine,
            NodeShape::Cylinder,
            NodeShape::Circle,
            NodeShape::DoubleCircle,
            NodeShape::Ellipse,
            NodeShape::Asymmetric,
            NodeShape::Rhombus,
            NodeShape::Hexagon,
            NodeShape::ParallelogramRight,
            NodeShape::ParallelogramLeft,
            NodeShape::TrapezoidUp,
            NodeShape::TrapezoidDown,
        ];
        for shape in shapes {
            let node = FlowchartNode {
                id: "A".into(),
                label: "ラベル".into(),
                markdown_label: false,
                shape: shape.clone(),
                classes: Vec::new(),
            };
            let source = format!("flowchart TD\n{INDENT}{}", node_declaration(&node));
            let document = parse_flowchart(&source)
                .unwrap_or_else(|error| panic!("{shape:?} should parse: {error}\n{source}"));
            assert_eq!(
                document.nodes.first().map(|node| node.shape.clone()),
                Some(shape.clone()),
                "shape of {source}"
            );
            assert_eq!(
                document.nodes.first().map(|node| node.label.clone()),
                Some("ラベル".to_string())
            );
            assert_renders(&source);
        }
    }

    #[test]
    fn labels_with_structural_characters_are_quoted_and_survive() {
        for label in [
            "かっこ (まる)",
            "角 [かっこ]",
            "波 {なみ}",
            "縦棒 | あり",
            "引用 \" あり",
            "斜線 / と \\",
            "セミコロン ; あり",
        ] {
            let node = FlowchartNode {
                id: "A".into(),
                label: label.into(),
                markdown_label: false,
                shape: NodeShape::Rectangle,
                classes: Vec::new(),
            };
            let source = format!("flowchart TD\n{INDENT}{}", node_declaration(&node));
            let document = parse_flowchart(&source)
                .unwrap_or_else(|error| panic!("`{label}` should parse: {error}\n{source}"));
            assert_eq!(
                document.nodes.first().map(|node| node.label.as_str()),
                Some(label),
                "label of {source}"
            );
            assert_renders(&source);
        }
    }

    #[test]
    fn edge_labels_with_pipes_are_quoted_and_survive() {
        let edge = FlowchartEdge {
            from: "A".into(),
            to: "B".into(),
            label: Some("は い | いいえ".into()),
            markdown_label: false,
            arrow: ArrowKind::Arrow,
            stroke: StrokeKind::Normal,
            length: 1,
        };
        let source = format!("flowchart TD\n{INDENT}{}", edge_statement(&edge));
        let document = parse_flowchart(&source).expect("quoted edge label should parse");
        assert_eq!(
            document.edges.first().and_then(|edge| edge.label.clone()),
            Some("は い | いいえ".to_string())
        );
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_statement() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nflowchart TD\n  %% 先頭のメモ\n  A --> B\n  %% 末尾のメモ\n  classDef warn fill:#f96";
        let document = parse_flowchart(source).expect("source should parse");
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
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 末尾のメモ".to_string()),
                (
                    PreservedPosition::Bottom,
                    "classDef warn fill:#f96".to_string()
                ),
            ]
        );

        let emitted = emit_flowchart(&document);
        let init_line = emitted.lines().next().unwrap_or_default();
        assert!(
            init_line.starts_with("%%{init"),
            "the init directive must stay before the diagram keyword:\n{emitted}"
        );
        assert!(
            emitted
                .lines()
                .nth(1)
                .unwrap_or_default()
                .starts_with("flowchart"),
            "{emitted}"
        );
        assert_stable(source);
    }

    /// `merman` marks a clicked node with a `clickable` class of its own. The
    /// `click` statement is preserved and marks it again on the next parse, so
    /// re-emitting the class would grow the diagram by one `clickable` per
    /// visit to the editor.
    #[test]
    fn a_click_statement_does_not_accumulate_its_class() {
        for source in [
            "flowchart TD\n  A --> B\n  click A \"https://example.com\" \"注記\"",
            "flowchart TD\n  subgraph g [まとまり]\n    A --> B\n  end\n  click g \"https://example.com\"",
        ] {
            let document = parse_flowchart(source).expect("source should parse");
            let emitted = assert_stable(source);
            assert!(
                !emitted.contains("clickable"),
                "the click statement already carries the class:\n{emitted}"
            );
            assert!(
                emitted.contains("click "),
                "the click statement itself must survive:\n{emitted}"
            );
            assert!(
                document
                    .nodes
                    .iter()
                    .all(|node| !node.classes.iter().any(|class| class == "clickable")),
                "for {source}"
            );
            assert_renders(&emitted);
        }
    }

    /// A user's own class called `clickable` is not the parser's, and has no
    /// statement of its own to bring it back.
    #[test]
    fn a_clickable_class_without_a_click_statement_survives() {
        let source = "flowchart TD\n  A --> B\n  class A clickable\n  classDef clickable fill:#f96";
        let emitted = assert_stable(source);
        assert!(
            emitted.contains("class A clickable"),
            "the assignment the user wrote should stay:\n{emitted}"
        );
    }

    /// Spellings that carry information neither the model nor the line scanner
    /// holds. Opening the editor on one and writing it back would drop it, so
    /// the parser refuses instead.
    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for source in [
            "flowchart TD\n  A e1@--> B",
            "flowchart TD\n  A e1@--> B\n  e1@{ animate: true }",
            "flowchart TD\n  A --> B %% 行末のメモ",
        ] {
            let error = parse_flowchart(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains("テキストのまま編集してください"),
                "unexpected message for `{source}`: {message}"
            );
        }

        // The same characters inside a label are the user's text, not syntax,
        // and the shape attribute they resemble is carried by the model.
        for source in [
            "flowchart TD\n  A[メール a@b.example] --> B",
            "flowchart TD\n  A[\"100%% 完了\"] --> B",
            "flowchart TD\n  A -->|a@b| B",
            "flowchart TD\n  A@{ shape: hourglass, label: \"砂時計\" } --> B",
        ] {
            assert!(
                parse_flowchart(source).is_ok(),
                "`{source}` should still open"
            );
        }
    }

    #[test]
    fn class_membership_survives_both_spellings() {
        for source in [
            "flowchart TD\n  A[あ]:::warn --> B\n  classDef warn fill:#f96",
            "flowchart TD\n  A[あ] --> B\n  class A warn\n  classDef warn fill:#f96",
        ] {
            let document = parse_flowchart(source).expect("source should parse");
            let node = document
                .nodes
                .first()
                .expect("the diagram should have a node");
            assert_eq!(node.classes, vec!["warn".to_string()], "for {source}");
            let emitted = assert_stable(source);
            assert!(
                emitted.contains("class A warn"),
                "the class assignment should be re-emitted once:\n{emitted}"
            );
            assert_eq!(
                emitted.matches("class A warn").count(),
                1,
                "the class assignment must not be applied twice:\n{emitted}"
            );
            assert_renders(&emitted);
        }
    }

    #[test]
    fn nested_subgraphs_stay_nested() {
        let source = "flowchart TD\n  subgraph outer [外]\n    subgraph inner [内]\n      A --> B\n    end\n  end";
        let document = parse_flowchart(source).expect("source should parse");
        let outer = document
            .subgraphs
            .iter()
            .position(|group| group.id == "outer")
            .expect("outer subgraph");
        let inner = document
            .subgraphs
            .iter()
            .position(|group| group.id == "inner")
            .expect("inner subgraph");
        assert_eq!(document.subgraphs[inner].parent, Some(outer));
        assert_eq!(document.subgraphs[outer].parent, None);

        let emitted = emit_flowchart(&document);
        let outer_line = emitted
            .lines()
            .position(|line| line.trim_start().starts_with("subgraph outer"))
            .expect("outer block");
        let inner_line = emitted
            .lines()
            .position(|line| line.trim_start().starts_with("subgraph inner"))
            .expect("inner block");
        assert!(
            outer_line < inner_line,
            "the inner block must be written inside the outer one:\n{emitted}"
        );
        assert_stable(source);
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_flowchart() {
        let document = parse_flowchart("").expect("an empty fence should parse");
        assert!(document.nodes.is_empty());
        assert!(document.edges.is_empty());
        assert_eq!(emit_flowchart(&document), "flowchart TB");
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "sequenceDiagram\n  A->>B: hi",
            "pie title x\n  \"a\" : 1",
            "classDiagram\n  class A",
        ] {
            assert!(!is_flowchart_source(source), "{source}");
            assert!(parse_flowchart(source).is_err(), "{source}");
        }
        assert!(is_flowchart_source("flowchart TD\n A --> B"));
        assert!(is_flowchart_source("%% note\ngraph LR\n A --> B"));
    }

    #[test]
    fn node_ids_are_validated() {
        assert!(is_valid_node_id("A"));
        assert!(is_valid_node_id("node_1"));
        assert!(!is_valid_node_id(""));
        assert!(!is_valid_node_id("a b"));
        assert!(!is_valid_node_id("a-b"));
        assert!(!is_valid_node_id("日本語"));
        assert!(!is_valid_node_id("end"));
        assert!(!is_valid_node_id("subgraph"));
    }

    #[test]
    fn unused_id_skips_taken_names() {
        let mut document = FlowchartDocument::empty();
        document.nodes.push(FlowchartNode {
            id: "N".into(),
            label: "N".into(),
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        document.nodes.push(FlowchartNode {
            id: "N1".into(),
            label: "N1".into(),
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        assert_eq!(document.unused_id("N"), "N2");
        assert_eq!(document.unused_id("M"), "M");
    }
}
