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
//!   directives, `style` / `click`, accessibility statements, and the
//!   `classDef` / `linkStyle` statements the form cannot hold (see below) —
//!   survives verbatim in [`FlowchartDocument::preserved_lines`], because
//!   dropping a user's styling on an unrelated edit would be worse than any
//!   amount of re-spelling.
//!
//! `classDef` statements are carried in [`FlowchartDocument::class_defs`] when
//! every one of them names a single class, and a `linkStyle` statement that
//! addresses a single link is carried on that link. The rest stay verbatim.
//!
//! Known losses, all of which concern constructs the editor does not expose:
//! a `class` statement that names an edge rather than a node or a subgraph —
//! the parser refuses such a diagram rather than dropping the class — and
//! per-node `style`, which is kept as a preserved statement but is not attached
//! to the node it styles.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::diagrams::flowchart::FlowchartModel;

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
    /// The id the source gave the edge (`A e1@--> B`), which is what an
    /// `e1@{ … }` statement addresses. `None` for an edge `merman` numbered
    /// itself: those ids are derived from the endpoints and would move onto
    /// another edge as soon as one is added beside it.
    pub user_id: Option<String>,
    pub label: Option<String>,
    pub markdown_label: bool,
    pub arrow: ArrowKind,
    pub stroke: StrokeKind,
    /// How many layout ranks the edge spans; `A --> B` is 1 and each extra dash
    /// adds one. Not editable, but rewriting every long edge as a short one
    /// would rearrange the picture.
    pub length: usize,
    /// What a `linkStyle <n> …` statement addressing only this edge says after
    /// the number, verbatim. `linkStyle` addresses an edge by position, so the
    /// number is written from wherever the edge ends up.
    pub link_style: Option<String>,
    /// The id of the subgraph block the edge was written in, so that it is
    /// written back there rather than at the top level. The emitter falls back
    /// to the top level whenever writing it inside would change the picture.
    pub subgraph: Option<String>,
}

/// A `classDef` statement that names a single class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowchartClassDef {
    pub name: String,
    /// The style list after the name, verbatim; it may be empty.
    pub style: String,
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
    /// Empty whenever the source has a `classDef` statement the form cannot
    /// hold, in which case every `classDef` stays in `preserved_lines`: moving
    /// only some of them would change the order the styles apply in.
    pub class_defs: Vec<FlowchartClassDef>,
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
            class_defs: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    pub fn node_index(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == id)
    }

    /// An id that no node, no subgraph and no edge is using, based on `prefix`.
    pub fn unused_id(&self, prefix: &str) -> String {
        let taken = |candidate: &str| {
            self.nodes.iter().any(|node| node.id == candidate)
                || self.subgraphs.iter().any(|group| group.id == candidate)
                || self
                    .edges
                    .iter()
                    .any(|edge| edge.user_id.as_deref() == Some(candidate))
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
    if let Some(problem) = crate::mermaid_model::preprocessing_problem_for_parsed_labels(source) {
        bail!("{problem}");
    }
    let scan = scan_source(source)?;

    let model = if source.trim().is_empty() {
        None
    } else {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| anyhow!("{error}"))?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        match parsed.into_parts().1 {
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
            class_defs: Vec::new(),
            preserved_lines: scan.preserved,
        });
    };

    let mut document = document_from_model(model, scan.keyword, scan.preserved, &scan.edge_ids);
    if let Some(problem) = unsupported_construct(&scan.statements, &document, &scan.edge_ids)
        .or_else(|| shared_edge_id(&document))
        .or_else(|| preserved_line_with_a_statement(&document.preserved_lines))
    {
        bail!("{problem}");
    }
    if !document.subgraphs.is_empty()
        && let Some(blocks) = edge_blocks(&scan.statements, &document)
    {
        for (edge, block) in document.edges.iter_mut().zip(blocks) {
            edge.subgraph = block;
        }
    }
    lift_class_defs(&mut document);
    lift_link_styles(&mut document);
    Ok(document)
}

fn document_from_model(
    model: FlowchartModel,
    keyword: FlowchartKeyword,
    preserved: Vec<PreservedLine>,
    edge_ids: &[String],
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
            // `merman` puts a user's own id in the same field as the one it
            // generates and does not say which happened, so the spellings the
            // source actually contains decide.
            user_id: edge_ids.contains(&edge.id).then(|| edge.id.clone()),
            from: edge.from,
            to: edge.to,
            markdown_label: edge.label_type.as_deref() == Some("markdown"),
            label: edge.label,
            arrow: ArrowKind::from_model(edge.edge_type.as_deref()),
            stroke: StrokeKind::from_model(edge.stroke.as_deref()),
            length: edge.length.max(1),
            link_style: None,
            subgraph: None,
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
        class_defs: Vec::new(),
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

    let layout = Layout::new(document);
    for item in layout.top_level() {
        match item {
            LayoutItem::Edge(index) => {
                if let Some(edge) = document.edges.get(index) {
                    let _ = writeln!(out, "{INDENT}{}", edge_statement(edge));
                }
            }
            LayoutItem::Block(index) => emit_subgraph(document, &layout, index, 1, &mut out),
        }
    }

    for (id, classes) in class_assignments(document) {
        let _ = writeln!(out, "{INDENT}class {id} {}", classes.join(","));
    }

    for definition in &document.class_defs {
        if definition.style.is_empty() {
            let _ = writeln!(out, "{INDENT}classDef {}", definition.name);
        } else {
            let _ = writeln!(
                out,
                "{INDENT}classDef {} {}",
                definition.name, definition.style
            );
        }
    }

    for (index, edge) in document.edges.iter().enumerate() {
        if let Some(style) = &edge.link_style {
            let _ = writeln!(out, "{INDENT}linkStyle {index} {style}");
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
    layout: &Layout,
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
    for child in layout.children_of(Some(index)) {
        emit_subgraph(document, layout, *child, depth + 1, out);
    }
    for node_id in &group.node_ids {
        // A nested block is a member of its parent under its own id; writing
        // that id as a bare statement as well would declare a node beside it.
        if document.subgraphs.iter().any(|group| group.id == *node_id) {
            continue;
        }
        let _ = writeln!(out, "{inner}{node_id}");
    }
    // The edges come after the bare members so that the endpoints they
    // mention are members already, and the member order — which the layout
    // reads — stays the one the list above spells out.
    for edge_index in layout.edges_in(index) {
        if let Some(edge) = document.edges.get(edge_index) {
            let _ = writeln!(out, "{inner}{}", edge_statement(edge));
        }
    }
    let _ = writeln!(out, "{indent}end");
}

/// One entry of the top level in the order it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutItem {
    Edge(usize),
    Block(usize),
}

/// Where each edge and each subgraph block is written.
///
/// `merman` numbers the edges in the order the statements declare them, and
/// both `linkStyle` and the layout read that order, so an edge is only written
/// inside the block it came from when every edge still comes out in the
/// document's order. Otherwise every edge goes to the top level, which says the
/// same thing with a wider diff.
struct Layout {
    children: HashMap<Option<usize>, Vec<usize>>,
    /// For each edge, the block it is written in.
    placements: Vec<Option<usize>>,
}

impl Layout {
    fn new(document: &FlowchartDocument) -> Self {
        let mut layout = Self {
            children: subgraph_children(document),
            placements: vec![None; document.edges.len()],
        };
        let requested = layout.requested_placements(document);
        if requested.iter().any(Option::is_some) {
            let top_level = std::mem::replace(&mut layout.placements, requested);
            if !layout.keeps_edge_order() {
                layout.placements = top_level;
            }
        }
        layout
    }

    fn children_of(&self, parent: Option<usize>) -> &[usize] {
        self.children
            .get(&parent)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn edges_in(&self, block: usize) -> impl Iterator<Item = usize> + '_ {
        self.placements
            .iter()
            .enumerate()
            .filter(move |(_, placement)| **placement == Some(block))
            .map(|(index, _)| index)
    }

    /// The block each edge asks for, where writing it there leaves every
    /// subgraph's members as they are.
    ///
    /// An edge statement makes its endpoints members of the block it sits in,
    /// unless a block that closed earlier already holds them. The endpoints
    /// therefore have to be members of the block already, or of one closed
    /// before it.
    fn requested_placements(&self, document: &FlowchartDocument) -> Vec<Option<usize>> {
        let closing_order = self.closing_order();
        let rank = |block: usize| closing_order.iter().position(|index| *index == block);
        document
            .edges
            .iter()
            .map(|edge| {
                let block = document
                    .subgraphs
                    .iter()
                    .position(|group| Some(&group.id) == edge.subgraph.as_ref())?;
                let group = document.subgraphs.get(block)?;
                let block_rank = rank(block)?;
                let claimed = |id: &str| {
                    group.node_ids.iter().any(|member| member == id)
                        || document
                            .subgraphs
                            .iter()
                            .enumerate()
                            .any(|(other, earlier)| {
                                rank(other).is_some_and(|other_rank| other_rank < block_rank)
                                    && earlier.node_ids.iter().any(|member| member == id)
                            })
                };
                (claimed(&edge.from) && claimed(&edge.to)).then_some(block)
            })
            .collect()
    }

    /// The blocks in the order their `end` lines are written.
    fn closing_order(&self) -> Vec<usize> {
        let mut order = Vec::new();
        for block in self.children_of(None) {
            self.push_closing_order(*block, &mut order);
        }
        order
    }

    fn push_closing_order(&self, block: usize, order: &mut Vec<usize>) {
        for child in self.children_of(Some(block)) {
            self.push_closing_order(*child, order);
        }
        order.push(block);
    }

    /// The top level of the document, after the node declarations.
    ///
    /// Top level edges keep their order among themselves and so do the
    /// blocks. A block is written just before the first edge that has to come
    /// out of it, and a block without edges as late as possible, which for a
    /// document without edges in its blocks is after every edge.
    fn top_level(&self) -> Vec<LayoutItem> {
        let blocks = self.children_of(None);
        let mut items = Vec::new();
        let mut next_block = 0;
        for (edge, placement) in self.placements.iter().enumerate() {
            let Some(block) = placement else {
                items.push(LayoutItem::Edge(edge));
                continue;
            };
            let top = self.top_level_block(*block);
            let Some(position) = blocks.iter().position(|candidate| *candidate == top) else {
                continue;
            };
            while next_block <= position {
                if let Some(block) = blocks.get(next_block) {
                    items.push(LayoutItem::Block(*block));
                }
                next_block += 1;
            }
        }
        for block in blocks.iter().skip(next_block) {
            items.push(LayoutItem::Block(*block));
        }
        items
    }

    fn top_level_block(&self, block: usize) -> usize {
        let mut current = block;
        while let Some(parent) = self
            .children
            .iter()
            .find(|(_, children)| children.contains(&current))
            .and_then(|(parent, _)| *parent)
        {
            current = parent;
        }
        current
    }

    /// Whether writing the edges where the placements put them numbers them in
    /// the document's order.
    fn keeps_edge_order(&self) -> bool {
        let mut order = Vec::new();
        for item in self.top_level() {
            match item {
                LayoutItem::Edge(edge) => order.push(edge),
                LayoutItem::Block(block) => self.push_block_edges(block, &mut order),
            }
        }
        order.len() == self.placements.len()
            && order.iter().enumerate().all(|(index, edge)| index == *edge)
    }

    fn push_block_edges(&self, block: usize, order: &mut Vec<usize>) {
        for child in self.children_of(Some(block)) {
            self.push_block_edges(*child, order);
        }
        order.extend(self.edges_in(block));
    }
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
    let mut operator = edge_operator(edge.arrow, edge.stroke, edge.length);
    // Mermaid writes a user defined edge id between the source node and the
    // link operator, which is also where the parser looks for it.
    if let Some(id) = &edge.user_id {
        operator = format!("{id}@{operator}");
    }
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
    // Mermaid refuses `A[""]`; a quoted space is read as the same empty label
    // by both it and `merman`, which trim it.
    if label.is_empty() {
        return "\" \"".to_string();
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
///
/// The model holds labels with their entities already decoded, so a `#…;` in
/// one has to be written with its `#` encoded, or it would be decoded a second
/// time. `%%{` would open a directive that Mermaid cuts out of the text,
/// quotes or not, together with everything up to the next `}%%`.
fn escape_quoted(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for (offset, character) in label.char_indices() {
        let rest = label.get(offset + 1..).unwrap_or_default();
        if character == '#' && starts_entity_name(rest) {
            out.push_str("#35;");
        } else {
            out.push(character);
        }
    }
    out.replace("%%{", "%#37;{")
        .replace('"', "#quot;")
        .replace('|', "#124;")
        .replace('\\', "#92;")
        .replace("\r\n", "<br/>")
        .replace(['\n', '\r'], "<br/>")
}

/// Whether `rest`, the text after a `#`, makes that `#` an entity shorthand:
/// a run of word characters closed by `;`.
fn starts_entity_name(rest: &str) -> bool {
    let name_length = rest
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(rest.len());
    name_length > 0
        && rest
            .get(name_length..)
            .is_some_and(|after| after.starts_with(';'))
}

/// The class `merman` puts on the target of a `click` statement.
///
/// It is not spelled anywhere in the source — the statement itself puts it back
/// on every parse — so the editor neither offers it nor writes it out.
pub const CLICK_CLASS: &str = "clickable";

/// The class names the document's `classDef` statements declare, in the order
/// they were written, which is what the editor offers when a class is assigned.
///
/// `classDef` takes a comma separated list of names, and the statements
/// themselves are kept verbatim, so the names are read back off those lines.
pub fn declared_class_names(document: &FlowchartDocument) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for definition in &document.class_defs {
        if definition.name != CLICK_CLASS && !names.contains(&definition.name) {
            names.push(definition.name.clone());
        }
    }
    for preserved in &document.preserved_lines {
        for line in preserved.text.lines() {
            let Some(rest) = line.trim().strip_prefix("classDef") else {
                continue;
            };
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let Some(list) = rest.split_whitespace().next() else {
                continue;
            };
            for name in list.trim_end_matches(';').split(',') {
                let name = name.trim();
                if is_valid_class_name(name)
                    && name != CLICK_CLASS
                    && !names.iter().any(|known| known == name)
                {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Whether `name` can be written as a bare Mermaid class name, i.e. whether a
/// `class A <name>` statement built from it would say what it looks like.
///
/// Mermaid reads a class name up to the first separator and never quotes it, so
/// the test is about what would end the name early rather than about which
/// alphabet it is in: a Japanese class name is as good as an ASCII one.
pub fn is_valid_class_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    ',' | ';'
                        | ':'
                        | '#'
                        | '%'
                        | '"'
                        | '\''
                        | '|'
                        | '['
                        | ']'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                )
        })
}

/// The id in a `foo@{ … }` statement, which addresses a node's shape when `foo`
/// names a node and an edge's metadata when it names an edge.
pub fn shape_data_target(line: &str) -> Option<&str> {
    let (id, rest) = line.trim().split_once('@')?;
    if !rest.trim_start().starts_with('{') {
        return None;
    }
    let id = id.trim();
    is_valid_node_id(id).then_some(id)
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
///
/// `callback` is not one of them: it starts no statement, and treating it as a
/// reserved id left a diagram that spells `callback A myFn` — which `merman`
/// reads as three node declarations — unconfirmable from the form.
fn is_reserved_id(id: &str) -> bool {
    const RESERVED: [&str; 11] = [
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
    /// The edge ids the source spells out, in the order they appear, including
    /// any it repeats.
    edge_ids: Vec<String>,
}

/// The ids the source gives its links, as in `A e1@--> B`.
///
/// The model cannot answer this: `merman` writes a user's id into the same
/// field as the `L_from_to_n` it invents for the rest. What the source contains
/// can, so the ids are read off the text and matched against the model
/// afterwards.
///
/// `lines` is the statement region, i.e. what follows the header. Nothing
/// before it declares a link, and an arrow spelled in the front matter or a
/// directive would otherwise be read as one.
fn declared_edge_ids(lines: &[&str]) -> Vec<String> {
    let mut ids = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("%%") || preserved_keyword(trimmed) {
            continue;
        }
        let outside = outside_labels(trimmed);
        for (at, _) in outside.match_indices('@') {
            // `@{` opens shape data, whose id may well be a node's, and an `@`
            // with no link after it is text: an unquoted edge label such as
            // `A -- mail@example.com --> B` is not blanked out above.
            if !outside.get(at + 1..).is_some_and(opens_a_link) {
                continue;
            }
            let Some(id) = outside
                .get(..at)
                .and_then(|before| before.split_whitespace().next_back())
            else {
                continue;
            };
            if is_valid_node_id(id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
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
/// it twice. `callback` is absent for the opposite reason: Mermaid has no
/// statement of that name — the word only appears inside `click A callback fn`
/// — so `merman` reads a line that opens with it as bare node declarations,
/// which the model does carry.
fn preserved_keyword(line: &str) -> bool {
    const KEYWORDS: [&str; 6] = [
        "classDef",
        "style",
        "linkStyle",
        "click",
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

    // Only the statements after the header declare a link. Front matter, a
    // comment and a `%%{ … }%%` directive can all spell something that reads
    // like `id@-->`, and taking one for an edge id would refuse a diagram that
    // has no edge id at all.
    let edge_ids = declared_edge_ids(lines.get(index..).unwrap_or_default());

    let Some(keyword) = keyword else {
        // An empty fence: the editor opens on a blank flowchart, and whatever
        // comments the user left keep their place.
        return Ok(SourceScan {
            keyword: FlowchartKeyword::Flowchart,
            preserved,
            statements: Vec::new(),
            edge_ids,
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
        // `e1@{ animate: true }` addresses an edge, and the model carries
        // nothing of it but the id, so the statement is kept verbatim. The same
        // spelling on a node is its shape, which the model does carry.
        let addresses_an_edge =
            shape_data_target(trimmed).is_some_and(|target| edge_ids.iter().any(|id| id == target));
        if preserved_keyword(trimmed) || addresses_an_edge {
            let start = index - 1;
            if trimmed.ends_with('{') {
                while index < lines.len() && lines[index].trim() != "}" {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            } else {
                // A quoted string (a `click` tooltip) may run onto the lines
                // after it, which `merman` reads as part of the statement.
                let mut quotes = trimmed.matches('"').count();
                while quotes % 2 == 1 && index < lines.len() {
                    quotes += lines[index].matches('"').count();
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
        edge_ids,
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
fn unsupported_construct(
    statements: &[String],
    document: &FlowchartDocument,
    edge_ids: &[String],
) -> Option<String> {
    // A repeated edge id is the one the model silently drops: `merman` keeps
    // the first and numbers the rest itself, so writing the diagram back would
    // move an `e1@{ … }` statement onto a different link.
    let mut seen: Vec<&str> = Vec::new();
    for id in edge_ids {
        if seen.contains(&id.as_str()) {
            return Some(format!(
                "エッジ ID「{id}」が重複している図は構造化編集に対応していません。テキストのまま編集してください。"
            ));
        }
        seen.push(id);
    }

    for statement in statements {
        let outside = outside_labels(statement);
        if outside.contains("%%") {
            return Some(format!(
                "文の末尾のコメントは構造化編集で失われます ({statement})。コメントを独立した行に移すか、テキストのまま編集してください。"
            ));
        }
        // Classes are carried per node and per subgraph; Mermaid also lets a
        // `class` statement name an edge, and that the model has nowhere to put.
        if let Some(rest) = statement.strip_prefix("class")
            && rest.starts_with(char::is_whitespace)
            && let Some(targets) = rest.split_whitespace().next()
            && let Some(edge) = targets
                .split(',')
                .find(|target| edge_ids.iter().any(|id| id == target.trim()))
        {
            return Some(format!(
                "エッジ ({edge}) にクラスを付けた図は構造化編集に対応していません。テキストのまま編集してください。"
            ));
        }
        for (at, _) in outside.match_indices('@') {
            let target = outside
                .get(..at)
                .unwrap_or_default()
                .split_whitespace()
                .next_back()
                .unwrap_or_default();
            let after = outside.get(at + 1..).unwrap_or_default();
            // `merman` only reads an `@` as syntax where shape data or a link
            // follows it. Anywhere else the diagram would not have parsed
            // unless the `@` sat in an unquoted edge label, which the model
            // carries as text.
            if !after.starts_with(['{', '"']) && !opens_a_link(after) {
                continue;
            }
            // `id@{ … }` on a node is the Mermaid 11 shape spelling, which the
            // model carries. On an edge it is metadata the scanner keeps
            // verbatim, and the line scanner has already taken it out of the
            // statements, so reaching one here means the id names neither.
            if after.trim_start().starts_with('{') && document.node_index(target).is_some() {
                // The model carries a node's shape and label; an icon, an
                // image and their placement would be dropped on the way back.
                let body = after
                    .trim_start()
                    .get(1..)
                    .unwrap_or_default()
                    .split('}')
                    .next()
                    .unwrap_or_default();
                if let Some(key) = body
                    .split(',')
                    .filter_map(|pair| pair.split(':').next())
                    .map(str::trim)
                    .find(|key| !key.is_empty() && !matches!(*key, "shape" | "label"))
                {
                    return Some(format!(
                        "要素 `{target}` の `{key}:` は構造化編集で失われます ({statement})。テキストのまま編集してください。"
                    ));
                }
                continue;
            }
            // `A e1@--> B` names the link that follows it, which is carried.
            if !after.trim_start().starts_with('{') && edge_ids.iter().any(|id| id == target) {
                continue;
            }
            return Some(format!(
                "エッジ ID (`{target}@`) を使った図は構造化編集に対応していません ({statement})。テキストのまま編集してください。"
            ));
        }
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

/// Whether `rest` — what follows an `@` — opens a link, which is what makes the
/// word before the `@` an edge id rather than text.
fn opens_a_link(rest: &str) -> bool {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(['x', 'o', '<']).unwrap_or(rest);
    rest.starts_with("--")
        || rest.starts_with("==")
        || rest.starts_with("-.")
        || rest.starts_with("~~~")
}

/// An edge id that is also a node's or a subgraph's id, which the parser
/// refuses.
///
/// `merman` sends an `id@{ … }` statement to the edge whenever an edge has that
/// id, so a node of the same name whose shape the emitter writes that way would
/// lose it, and the statement that was meant for the edge would decide what
/// happens to the node once the edge is deleted.
fn shared_edge_id(document: &FlowchartDocument) -> Option<String> {
    let id = document.edges.iter().find_map(|edge| {
        let id = edge.user_id.as_deref()?;
        (document.node_index(id).is_some() || document.subgraphs.iter().any(|group| group.id == id))
            .then_some(id)
    })?;
    Some(format!(
        "エッジ ID「{id}」がノードまたはグループの ID と同じ図は構造化編集に対応していません。テキストのまま編集してください。"
    ))
}

/// A kept line that carries a statement the model also reads, which the parser
/// refuses.
///
/// A statement ends at `;` as well as at the end of the line, so
/// `style A fill:#f00; A --> C` keeps the edge in the verbatim line while the
/// model carries it too, and writing both back would draw it twice. Kept
/// statements chained after one another are fine: nothing else holds them.
fn preserved_line_with_a_statement(preserved: &[PreservedLine]) -> Option<String> {
    for line in preserved {
        let trimmed = line.text.trim();
        // Front matter, directives, comments and edge metadata are not
        // statements, and the accessibility statements are taken out of the
        // source before `merman` splits it at `;`.
        if line.position == PreservedPosition::Preamble
            || !preserved_keyword(trimmed)
            || trimmed.starts_with("acc")
        {
            continue;
        }
        let chained_statement = statement_segments(trimmed)
            .into_iter()
            .skip(1)
            .any(|segment| {
                let segment = segment.trim();
                !segment.is_empty() && !preserved_keyword(segment)
            });
        if chained_statement {
            return Some(format!(
                "1 行に続けて書かれた文 ({trimmed}) は構造化編集で二重に書き戻されます。文を別の行に分けるか、テキストのまま編集してください。"
            ));
        }
    }
    None
}

/// `line` split at every `;` that ends a statement, i.e. every one outside a
/// quoted string.
fn statement_segments(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, ';') => {
                segments.push(line.get(start..index).unwrap_or_default());
                start = index + 1;
            }
            (None, _) => {}
        }
    }
    segments.push(line.get(start..).unwrap_or_default());
    segments
}

/// Whether `text` can follow a `classDef` name or a `linkStyle` number without
/// changing what the statement says or starting another one.
///
/// A statement ends at `;` or at the end of the line, a quote left open runs on
/// to the end of the diagram, and `%%` is not a comment in the middle of a line.
/// One `;` at the very end only ends the statement where the line ends anyway,
/// and is kept as written: `merman` reads `stroke:#f00;` differently from
/// `stroke:#f00` in a `linkStyle`, since it takes `#f00;` for an entity.
///
/// A kept line that ends with `{` opens a block the line scanner reads up to
/// the next `}` line, so a style ending with one would pull the statements
/// written after it — the next `linkStyle`, say — into a verbatim block that
/// no longer follows its edge.
pub fn is_valid_style_text(text: &str) -> bool {
    if text.trim_end().ends_with('{') {
        return false;
    }
    let text = text.strip_suffix(';').unwrap_or(text);
    !text.contains(['\n', '\r', ';'])
        && !text.contains("%%")
        && text.matches('"').count().is_multiple_of(2)
        && text.matches('\'').count().is_multiple_of(2)
        && !text
            .chars()
            .any(|character| character.is_whitespace() && character != ' ' && character != '\t')
}

/// `line` without the keyword it opens with, when it is a single line that opens
/// with `keyword` followed by a space or a tab.
///
/// `merman` separates a statement's words with ASCII whitespace only, so a line
/// with any other whitespace in it is left verbatim rather than read in a way
/// the parser would not.
fn single_line_statement<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    if line.contains('\n')
        || line
            .chars()
            .any(|character| character.is_whitespace() && character != ' ' && character != '\t')
    {
        return None;
    }
    let rest = line.trim().strip_prefix(keyword)?;
    rest.starts_with([' ', '\t']).then(|| rest.trim_start())
}

/// The word `rest` opens with, up to the first space or tab, and what follows
/// it.
fn first_word(rest: &str) -> (&str, &str) {
    match rest.find([' ', '\t']) {
        Some(end) => (
            rest.get(..end).unwrap_or_default(),
            rest.get(end..).unwrap_or_default().trim(),
        ),
        None => (rest, ""),
    }
}

/// The style list of a statement, verbatim.
fn style_text(rest: &str) -> Option<&str> {
    let style = rest.trim_end();
    is_valid_style_text(style).then_some(style)
}

fn class_def_statement(line: &str) -> Option<FlowchartClassDef> {
    let rest = single_line_statement(line, "classDef")?;
    let (name, style) = first_word(rest);
    if !is_valid_class_name(name) {
        return None;
    }
    Some(FlowchartClassDef {
        name: name.to_string(),
        style: style_text(style)?.to_string(),
    })
}

fn is_class_def_line(line: &str) -> bool {
    line.trim()
        .strip_prefix("classDef")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Moves the `classDef` statements into [`FlowchartDocument::class_defs`] when
/// every one of them names a single class, once.
///
/// `merman` keeps the definitions in the order they were first written, and
/// that is the order their styles are applied in. Carrying some of them and
/// keeping the rest verbatim would write them back in a different order, so a
/// single statement the form cannot hold keeps all of them verbatim.
fn lift_class_defs(document: &mut FlowchartDocument) {
    let mut definitions: Vec<FlowchartClassDef> = Vec::new();
    let mut lines = Vec::new();
    for (index, line) in document.preserved_lines.iter().enumerate() {
        if !is_class_def_line(&line.text) {
            continue;
        }
        let Some(definition) = class_def_statement(&line.text) else {
            return;
        };
        if definitions
            .iter()
            .any(|known| known.name == definition.name)
        {
            return;
        }
        definitions.push(definition);
        lines.push(index);
    }
    for index in lines.into_iter().rev() {
        document.preserved_lines.remove(index);
    }
    document.class_defs = definitions;
}

/// The positions a `linkStyle` line addresses, or `None` for `linkStyle
/// default` and for a line that is not a `linkStyle` statement.
fn link_style_positions(line: &str) -> Option<Vec<usize>> {
    let rest = line.trim().strip_prefix("linkStyle")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let list = rest.split_whitespace().next()?;
    if list == "default" {
        return None;
    }
    list.split(',')
        .map(|entry| entry.trim().parse::<usize>().ok())
        .collect()
}

/// The positions the `linkStyle` statements kept verbatim address. The form
/// cannot edit the styling of those edges, since the kept statement would
/// apply alongside whatever it wrote.
pub fn preserved_link_style_positions(document: &FlowchartDocument) -> Vec<usize> {
    let mut positions = document
        .preserved_lines
        .iter()
        .filter_map(|line| link_style_positions(&line.text))
        .flatten()
        .collect::<Vec<_>>();
    positions.sort_unstable();
    positions.dedup();
    positions
}

fn link_style_statement(line: &str) -> Option<(usize, String)> {
    let rest = single_line_statement(line, "linkStyle")?;
    let (number, style) = first_word(rest);
    if !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = number.parse::<usize>().ok()?;
    let style = style_text(style)?;
    (!style.is_empty() && !style.starts_with(',')).then(|| (number, style.to_string()))
}

/// Moves each `linkStyle` statement that is the only one addressing its edge
/// onto that edge.
///
/// The carried ones are written after the kept lines' `e1@{ … }` statements,
/// and both can set an edge's curve; a statement that does so for an edge with
/// such a line stays where it was, so that the last word stays the same one.
fn lift_link_styles(document: &mut FlowchartDocument) {
    let mut addressed: HashMap<usize, usize> = HashMap::new();
    for line in &document.preserved_lines {
        for position in link_style_positions(&line.text).into_iter().flatten() {
            *addressed.entry(position).or_default() += 1;
        }
    }
    let with_metadata = document
        .preserved_lines
        .iter()
        .filter_map(|line| shape_data_target(&line.text))
        .map(str::to_string)
        .collect::<Vec<_>>();

    let mut lifted = Vec::new();
    for (index, line) in document.preserved_lines.iter().enumerate() {
        let Some((position, style)) = link_style_statement(&line.text) else {
            continue;
        };
        let Some(edge) = document.edges.get(position) else {
            continue;
        };
        if addressed.get(&position) != Some(&1) {
            continue;
        }
        let curve_meets_metadata = style.contains("interpolate")
            && edge
                .user_id
                .as_ref()
                .is_some_and(|id| with_metadata.contains(id));
        if curve_meets_metadata {
            continue;
        }
        lifted.push((index, position, style));
    }
    for (index, position, style) in lifted.into_iter().rev() {
        document.preserved_lines.remove(index);
        if let Some(edge) = document.edges.get_mut(position) {
            edge.link_style = Some(style);
        }
    }
}

/// The subgraph block each edge was written in, as the id of the innermost
/// one, or `None` when the source cannot be read that way with confidence.
///
/// The model does not say, so the statements are read in order: a
/// `subgraph` line opens a block, `end` closes it — and `merman` lists the
/// blocks in the order they close — and every other statement is handed to the
/// parser on its own to count the edges it declares. The answer is only used
/// when those edges are exactly the model's, in the model's order.
fn edge_blocks(statements: &[String], document: &FlowchartDocument) -> Option<Vec<Option<String>>> {
    let mut open: Vec<usize> = Vec::new();
    let mut closing: Vec<Option<usize>> = Vec::new();
    let mut closed = 0;
    let mut owners: Vec<Option<usize>> = Vec::new();
    let mut endpoints: Vec<(String, String)> = Vec::new();

    for statement in statements {
        let statement = statement.trim();
        if statement
            .strip_prefix("subgraph")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            // A header followed by more statements on the same line would put
            // those in the block without a line of their own.
            if statement.contains(';') {
                return None;
            }
            open.push(closing.len());
            closing.push(None);
            continue;
        }
        if statement == "end" || statement == "end;" {
            let block = open.pop()?;
            *closing.get_mut(block)? = Some(closed);
            closed += 1;
            continue;
        }
        if statement
            .strip_prefix("direction")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        {
            continue;
        }
        // Every link operator is spelled with one of these.
        if !statement.contains(['-', '=', '~']) {
            continue;
        }
        let parsed =
            crate::parse_for_render_model(&format!("flowchart TD\n{statement}")).ok()??;
        let RenderSemanticModel::Flowchart(model) = parsed.into_parts().1 else {
            return None;
        };
        for edge in model.edges {
            owners.push(open.last().copied());
            endpoints.push((edge.from, edge.to));
        }
    }

    if !open.is_empty()
        || closed != document.subgraphs.len()
        || endpoints.len() != document.edges.len()
        || document
            .edges
            .iter()
            .zip(&endpoints)
            .any(|(edge, (from, to))| edge.from != *from || edge.to != *to)
    {
        return None;
    }
    Some(
        owners
            .into_iter()
            .map(|owner| {
                let closed_at = (*closing.get(owner?)?)?;
                Some(document.subgraphs.get(closed_at)?.id.clone())
            })
            .collect(),
    )
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

    /// Found by writing `merman`'s examples back and asking Mermaid 11.16.1
    /// to read them: an empty label written as `""`, which Mermaid refuses,
    /// an icon dropped from a node's shape data, and the continuation lines of
    /// a quoted `click` tooltip dropped from the statement.
    #[test]
    fn what_mermaid_refuses_or_the_model_would_lose_is_not_written() {
        let emitted = assert_stable("graph LR\n  A(( )) -->|step 1| B(( ))");
        assert!(emitted.contains("A((\" \"))"), "{emitted}");
        assert!(!emitted.contains("\"\""), "{emitted}");

        let error = parse_flowchart(
            "flowchart TD\n    A@{ icon: \"fa:user\", form: \"square\", label: \"User Icon\" }",
        )
        .expect_err("the icon would be dropped");
        assert!(error.to_string().contains("`icon:`"), "{error}");
        assert_stable("flowchart TD\n    A@{ shape: rounded, label: \"Kept\" }");

        let source = "flowchart LR\n  A --> B\nclick A \"/x\" \"AAA\n6000\"";
        let emitted = assert_stable(source);
        assert!(
            emitted.contains("click A \"/x\" \"AAA\n6000\""),
            "{emitted}"
        );
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
            if shape == NodeShape::Ellipse {
                // Mermaid parses `(-…-)` but has no ellipse to draw it with
                // ("No such shape: ellipse"); `merman` 0.7 drew one anyway, 0.8
                // fails the way Mermaid does. The shape menu does not offer it.
                assert!(!NodeShape::PRESETS.contains(&shape));
                let error = render_to_svg(&source, &MermaidTheme::default())
                    .expect_err("merman 0.8 has no ellipse to draw");
                assert!(format!("{error:#}").contains("No such shape"), "{error:#}");
            } else {
                assert_renders(&source);
            }
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
            user_id: None,
            label: Some("は い | いいえ".into()),
            markdown_label: false,
            arrow: ArrowKind::Arrow,
            stroke: StrokeKind::Normal,
            length: 1,
            link_style: None,
            subgraph: None,
        };
        let source = format!("flowchart TD\n{INDENT}{}", edge_statement(&edge));
        let document = parse_flowchart(&source).expect("quoted edge label should parse");
        assert_eq!(
            document.edges.first().and_then(|edge| edge.label.clone()),
            Some("は い | いいえ".to_string())
        );
    }

    /// The last line used to be a `classDef`, which the model now carries in
    /// `class_defs` rather than as a kept line; `style` is still kept verbatim.
    #[test]
    fn preserved_lines_keep_their_side_of_the_first_statement() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nflowchart TD\n  %% 先頭のメモ\n  A --> B\n  %% 末尾のメモ\n  style B fill:#f96";
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
                (PreservedPosition::Bottom, "style B fill:#f96".to_string()),
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
            "flowchart TD\n  A --> B %% 行末のメモ",
            // The second `e1` is not the user's any more: `merman` numbers it
            // itself, so `e1@{ … }` would land on the first link alone.
            "flowchart TD\n  A e1@--> B\n  B e1@--> C",
            "flowchart TD\n  A e1@--> B\n  class e1 warn\n  classDef warn stroke:#f96",
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

    /// A user's own edge id is the only handle an `e1@{ … }` statement has, so
    /// both the id and the statement have to come back out.
    #[test]
    fn a_user_defined_edge_id_and_its_metadata_survive() {
        let source = "flowchart TD\n  A e1@--> B\n  e1@{ animate: true }";
        let emitted = assert_stable(source);
        assert!(
            emitted.contains("A e1@--> B"),
            "the edge id should be written back:\n{emitted}"
        );
        assert!(
            emitted.contains("e1@{ animate: true }"),
            "the edge metadata should be kept verbatim:\n{emitted}"
        );

        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document.edges.first().and_then(|edge| edge.user_id.clone()),
            Some("e1".to_string())
        );
        // The node the statement resembles must not have been declared by it.
        assert_eq!(
            document
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        assert_renders(&emitted);
    }

    /// An id `merman` generated is derived from the endpoints and would follow
    /// the wrong link as soon as one is added, so it is not written back.
    #[test]
    fn a_generated_edge_id_is_not_written_back() {
        let document =
            parse_flowchart("flowchart TD\n  A --> B\n  B --> C").expect("source should parse");
        assert!(
            document.edges.iter().all(|edge| edge.user_id.is_none()),
            "no edge here has an id of its own: {:?}",
            document.edges
        );
        let emitted = emit_flowchart(&document);
        assert!(
            !emitted.contains('@'),
            "an invented id must not reach the source:\n{emitted}"
        );
    }

    /// A labelled link takes its id in the same place, and the label has to stay
    /// on the far side of it.
    #[test]
    fn an_edge_id_and_a_label_can_share_a_link() {
        let emitted = assert_stable("flowchart LR\n  A e1@-. 途中 .-> B");
        assert!(
            emitted.contains("A e1@-.->|途中| B"),
            "the id and the label should both survive:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn the_class_names_on_offer_come_from_the_class_def_lines() {
        let document = parse_flowchart(
            "flowchart TD\n  A --> B\n  classDef warn fill:#f96\n  classDef ok,done stroke:#3f3;\n  classDef 強調 stroke-width:2px\n  classDef clickable fill:#eee",
        )
        .expect("source should parse");
        assert_eq!(
            declared_class_names(&document),
            vec![
                "warn".to_string(),
                "ok".to_string(),
                "done".to_string(),
                "強調".to_string()
            ],
            "the synthetic class of a `click` statement is never offered"
        );
    }

    /// A class name is written into the statement as it stands, so a name that
    /// would end early there cannot be offered.
    #[test]
    fn a_class_name_may_be_japanese_but_not_a_separator() {
        for name in ["warn", "warn-1", "強調", "_a"] {
            assert!(is_valid_class_name(name), "`{name}` should be offered");
        }
        for name in ["", "two words", "a,b", "a;", "a:b", "#quot;"] {
            assert!(!is_valid_class_name(name), "`{name}` should be refused");
        }
    }

    /// Mermaid has no statement that opens with `callback` — the word only
    /// appears inside `click A callback fn` — and `merman` reads such a line as
    /// bare node declarations. Treating it as a statement of its own left the
    /// diagram unopenable: `callback` came back as a node whose id the editor
    /// called reserved.
    #[test]
    fn a_line_that_opens_with_callback_is_read_as_node_declarations() {
        let source = "flowchart TD\n  A --> B\n  callback A myFn";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "callback", "myFn"],
            "`merman` declares the words as nodes, and the editor has to agree"
        );
        assert!(
            document.preserved_lines.is_empty(),
            "keeping the line as well would declare the nodes twice: {:?}",
            document.preserved_lines
        );
        assert!(
            is_valid_node_id("callback"),
            "a node the parser produces has to be one the editor can write back"
        );
        let emitted = assert_stable(source);
        assert_renders(&emitted);
    }

    /// An `@` only opens an edge id where a link operator follows it. Anywhere
    /// else — a label, a node's shape data, a comment, the front matter — it is
    /// part of the text, and reading one as an id would divert a node's shape
    /// statement into the kept lines or refuse the diagram outright.
    #[test]
    fn an_at_that_opens_no_link_is_not_an_edge_id() {
        for source in [
            "flowchart TD\n  A -->|mail@x| B",
            "flowchart TD\n  A --> B\n  A@{ shape: circle }",
            "flowchart TD\n  A[\"e1@--> B\"] --> B",
            "flowchart TD\n  %% 迂回路は e1@--> C で書く\n  A --> B",
        ] {
            let document = parse_flowchart(source).expect("source should parse");
            assert!(
                document.edges.iter().all(|edge| edge.user_id.is_none()),
                "no link here is named: {:?}\n{source}",
                document.edges
            );
            assert_renders(&assert_stable(source));
        }

        // The shape has to reach the node rather than be kept as a line about
        // an edge that does not exist.
        let document = parse_flowchart("flowchart TD\n  A --> B\n  A@{ shape: circle }")
            .expect("source should parse");
        assert_eq!(
            document.nodes.first().map(|node| node.shape.clone()),
            Some(NodeShape::Circle)
        );
    }

    /// The front matter and the header are not statements, so an arrow spelled
    /// in one names no link — and must not make `class a …` look as though it
    /// were putting a class on an edge, which the parser refuses.
    #[test]
    fn an_arrow_in_the_front_matter_does_not_name_a_link() {
        let source = "---\ntitle: a@-->b\n---\nflowchart TD\n  a --> c\n  class a warn\n  classDef warn fill:#f96";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document
                .nodes
                .iter()
                .find(|node| node.id == "a")
                .map(|node| node.classes.clone()),
            Some(vec!["warn".to_string()])
        );
        assert_renders(&assert_stable(source));
    }

    /// An id sits in front of the link operator whatever the operator is, so
    /// every spelling of a link has to be recognised as one.
    #[test]
    fn an_edge_id_survives_on_every_kind_of_link() {
        for operator in ["-->", "==>", "-.->", "--o", "--x", "<-->", "~~~"] {
            let source = format!("flowchart TD\n  A e1@{operator} B");
            let document = parse_flowchart(&source)
                .unwrap_or_else(|error| panic!("`{operator}` should parse: {error}"));
            assert_eq!(
                document.edges.first().and_then(|edge| edge.user_id.clone()),
                Some("e1".to_string()),
                "`{operator}` should keep the id the source gave it"
            );
            let emitted = assert_stable(&source);
            assert!(
                emitted.contains("A e1@"),
                "`{operator}` should be written back with its id:\n{emitted}"
            );
            assert_renders(&emitted);
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

    /// The drawing of `source`, with the document id every drawing gets from
    /// a process wide counter replaced so that two drawings can be compared.
    fn render(source: &str) -> String {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"));
        let id = svg
            .strip_prefix("<svg id=\"")
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("the drawing should open with its id: {svg}"))
            .to_string();
        svg.replace(&id, "merman-drawing")
    }

    /// The same document with every edge written at the top level, which is
    /// what the emitter did before it kept edges in their blocks.
    fn emitted_at_the_top_level(document: &FlowchartDocument) -> String {
        let mut document = document.clone();
        for edge in &mut document.edges {
            edge.subgraph = None;
        }
        emit_flowchart(&document)
    }

    /// The same document with its carried `linkStyle` statements written back
    /// as kept lines, which is how the emitter wrote them before.
    fn emitted_with_link_styles_kept(document: &FlowchartDocument) -> String {
        let mut document = document.clone();
        for (index, edge) in document.edges.iter_mut().enumerate() {
            if let Some(style) = edge.link_style.take() {
                document.preserved_lines.push(PreservedLine {
                    position: PreservedPosition::Bottom,
                    text: format!("  linkStyle {index} {style}"),
                });
            }
        }
        emit_flowchart(&document)
    }

    fn lines_inside_block<'a>(emitted: &'a str, id: &str) -> Vec<&'a str> {
        let mut lines = emitted
            .lines()
            .skip_while(|line| !line.trim_start().starts_with(&format!("subgraph {id} ")));
        let Some(header) = lines.next() else {
            panic!("no block `{id}` in:\n{emitted}");
        };
        let indent = header.len() - header.trim_start().len();
        lines
            .take_while(|line| line.len() - line.trim_start().len() > indent)
            .map(str::trim)
            .collect()
    }

    #[test]
    fn edges_are_written_back_inside_the_block_they_came_from() {
        let source = "flowchart TD\n  subgraph one [グループ 1]\n    direction LR\n    A --> B\n  end\n  subgraph two [グループ 2]\n    C --> D\n  end\n  B --> C";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document
                .edges
                .iter()
                .map(|edge| edge.subgraph.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("one"), Some("two"), None]
        );

        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "flowchart TB\n    A\n    B\n    C\n    D\n    subgraph one [グループ 1]\n        direction LR\n        B\n        A\n        A --> B\n    end\n    subgraph two [グループ 2]\n        D\n        C\n        C --> D\n    end\n    B --> C"
        );

        // Where the edge is written must not change the picture: not against
        // the source, and not against writing every edge at the top level.
        let svg = render(&emitted);
        assert_eq!(svg, render(source), "the drawing moved against the source");
        assert_eq!(
            svg,
            render(&emitted_at_the_top_level(&document)),
            "the drawing moved against the top level spelling"
        );
    }

    #[test]
    fn an_edge_in_an_outer_block_stays_after_the_inner_one() {
        let source = "flowchart TD\n  subgraph outer [外]\n    subgraph inner [内]\n      A --> B\n    end\n    C --> A\n  end\n  D --> C";
        let document = parse_flowchart(source).expect("source should parse");
        let emitted = assert_stable(source);
        assert_eq!(
            lines_inside_block(&emitted, "outer"),
            vec![
                "subgraph inner [内]",
                "B",
                "A",
                "A --> B",
                "end",
                "C",
                "C --> A",
            ],
            "{emitted}"
        );
        assert_eq!(emitted.lines().last(), Some("    D --> C"), "{emitted}");
        assert_eq!(
            render(&emitted),
            render(&emitted_at_the_top_level(&document)),
            "the drawing should not depend on where the edges are written"
        );
    }

    /// `merman` numbers edges in the order they are written, and an edge can
    /// only be written in its block where that order survives. The source here
    /// declares the outer block's own edge before the inner block, which the
    /// block layout (inner blocks, members, then edges) cannot reproduce.
    #[test]
    fn edges_go_to_the_top_level_when_their_blocks_would_reorder_them() {
        let source = "flowchart TD\n  subgraph outer [外]\n    C --> D\n    subgraph inner [内]\n      A --> B\n    end\n  end";
        // The first document remembers the blocks the source wrote the edges
        // in, which the emitted text no longer does, so only the text is
        // compared for stability here.
        let emitted = emit_flowchart(&parse_flowchart(source).expect("source should parse"));
        assert_eq!(assert_stable(&emitted), emitted);
        assert!(
            lines_inside_block(&emitted, "outer")
                .iter()
                .all(|line| !line.contains("-->")),
            "no edge should be written inside a block:\n{emitted}"
        );
        let document = parse_flowchart(&emitted).expect("the emitted source should parse");
        assert_eq!(
            document
                .edges
                .iter()
                .map(|edge| (edge.from.as_str(), edge.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("C", "D"), ("A", "B")],
            "the edges should keep their order"
        );
        assert_renders(&emitted);

        // The same happens when the form moves an edge away from its block's
        // neighbours.
        let mut document = parse_flowchart(
            "flowchart TD\n  subgraph one [一]\n    A --> B\n  end\n  subgraph two [二]\n    C --> D\n  end\n  B --> C",
        )
        .expect("source should parse");
        document.edges.swap(0, 2);
        let emitted = emit_flowchart(&document);
        for block in ["one", "two"] {
            assert!(
                lines_inside_block(&emitted, block)
                    .iter()
                    .all(|line| !line.contains("-->")),
                "block `{block}` should hold no edge:\n{emitted}"
            );
        }
        let reparsed = parse_flowchart(&emitted).expect("the emitted source should parse");
        assert_eq!(
            reparsed
                .edges
                .iter()
                .map(|edge| (edge.from.as_str(), edge.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("B", "C"), ("C", "D"), ("A", "B")]
        );
    }

    /// An edge statement makes its endpoints members of the block it is in, so
    /// an edge whose endpoint is not a member stays at the top level.
    #[test]
    fn an_edge_to_a_node_outside_its_block_stays_at_the_top_level() {
        let mut document = parse_flowchart(
            "flowchart TD\n  subgraph one [一]\n    A --> B\n  end\n  subgraph two [二]\n    C --> D\n  end\n  B --> C",
        )
        .expect("source should parse");
        if let Some(edge) = document.edges.get_mut(2) {
            edge.subgraph = Some("one".to_string());
        }
        let emitted = emit_flowchart(&document);
        assert_eq!(
            lines_inside_block(&emitted, "one"),
            vec!["B", "A", "A --> B"],
            "{emitted}"
        );
        assert_eq!(emitted.lines().last(), Some("    B --> C"), "{emitted}");
        let reparsed = parse_flowchart(&emitted).expect("the emitted source should parse");
        assert_eq!(reparsed.subgraphs, document.subgraphs);
    }

    #[test]
    fn class_defs_are_carried_when_every_one_names_a_single_class() {
        let source = "flowchart TD\n  A --> B\n  classDef warn fill:#f96;\n  classDef ok stroke:#3f3,stroke-width:2px\n  classDef default fill:#eee\n  class A warn";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document.class_defs,
            vec![
                FlowchartClassDef {
                    name: "warn".into(),
                    style: "fill:#f96;".into()
                },
                FlowchartClassDef {
                    name: "ok".into(),
                    style: "stroke:#3f3,stroke-width:2px".into()
                },
                FlowchartClassDef {
                    name: "default".into(),
                    style: "fill:#eee".into()
                },
            ]
        );
        assert!(
            document
                .preserved_lines
                .iter()
                .all(|line| !line.text.contains("classDef")),
            "a carried definition must not be kept as well: {:?}",
            document.preserved_lines
        );
        let emitted = assert_stable(source);
        assert!(
            emitted.contains(
                "    classDef warn fill:#f96;\n    classDef ok stroke:#3f3,stroke-width:2px\n    classDef default fill:#eee"
            ),
            "the definitions should come back in their order:\n{emitted}"
        );
        assert_eq!(render(&emitted), render(source));
        assert_eq!(
            declared_class_names(&document),
            vec!["warn".to_string(), "ok".to_string(), "default".to_string()]
        );
    }

    /// Moving some definitions and keeping the others would change the order
    /// their styles apply in, so one the model cannot hold keeps all of them.
    #[test]
    fn class_defs_stay_verbatim_when_one_cannot_be_carried() {
        for (source, kept) in [
            (
                "flowchart TD\n  A --> B\n  classDef c stroke:#333\n  classDef a,b fill:#f96",
                vec!["  classDef c stroke:#333", "  classDef a,b fill:#f96"],
            ),
            (
                "flowchart TD\n  A --> B\n  classDef a fill:#f00\n  classDef a stroke:#00f",
                vec!["  classDef a fill:#f00", "  classDef a stroke:#00f"],
            ),
        ] {
            let document = parse_flowchart(source).expect("source should parse");
            assert!(document.class_defs.is_empty(), "for {source}");
            assert_eq!(
                document
                    .preserved_lines
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>(),
                kept,
                "for {source}"
            );
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn a_link_style_on_one_edge_is_carried_and_follows_it() {
        let source =
            "flowchart TD\n  A --> B\n  B --> C\n  linkStyle 1 stroke:#f00,stroke-width:2px;";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document
                .edges
                .iter()
                .map(|edge| edge.link_style.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("stroke:#f00,stroke-width:2px;")]
        );
        assert!(document.preserved_lines.is_empty());
        let emitted = assert_stable(source);
        assert!(
            emitted.ends_with("    linkStyle 1 stroke:#f00,stroke-width:2px;"),
            "{emitted}"
        );
        // The source declares its nodes in its edges, which numbers them
        // differently from the emitted declarations, so the drawing is compared
        // with the same document keeping its statements as written instead.
        assert_eq!(
            render(&emitted),
            render(&emitted_with_link_styles_kept(&document))
        );

        let mut moved = document;
        moved.edges.swap(0, 1);
        let emitted = emit_flowchart(&moved);
        assert!(
            emitted.ends_with("    linkStyle 0 stroke:#f00,stroke-width:2px;"),
            "the number should follow the edge:\n{emitted}"
        );
    }

    /// A statement that names several edges, or an edge that several
    /// statements name, stays verbatim: the form would otherwise write a second
    /// statement that applies alongside the first.
    #[test]
    fn a_shared_link_style_stays_verbatim() {
        let source = "flowchart TD\n  A --> B\n  B --> C\n  C --> D\n  linkStyle 0,1 stroke:#f00\n  linkStyle 1 stroke:#00f\n  linkStyle 2 stroke:#0f0\n  linkStyle default stroke:#999";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document
                .edges
                .iter()
                .map(|edge| edge.link_style.as_deref())
                .collect::<Vec<_>>(),
            vec![None, None, Some("stroke:#0f0")]
        );
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| line.text.trim())
                .collect::<Vec<_>>(),
            vec![
                "linkStyle 0,1 stroke:#f00",
                "linkStyle 1 stroke:#00f",
                "linkStyle default stroke:#999"
            ]
        );
        assert_eq!(preserved_link_style_positions(&document), vec![0, 1]);
        let emitted = assert_stable(source);
        // The source declares its nodes in its edges, which numbers them
        // differently from the emitted declarations, so the drawing is compared
        // with the same document keeping its statements as written instead.
        assert_eq!(
            render(&emitted),
            render(&emitted_with_link_styles_kept(&document))
        );
    }

    /// `e1@{ curve: … }` and `linkStyle … interpolate …` both set the curve,
    /// and the later one wins. The carried statements are written before the
    /// kept lines, so one that sets the curve of an edge with such a line stays
    /// where it was.
    #[test]
    fn a_link_style_curve_stays_beside_the_edge_metadata() {
        let source = "flowchart TD\n  A e1@--> B\n  e1@{ curve: basis }\n  linkStyle 0 interpolate linear stroke:#f00";
        let document = parse_flowchart(source).expect("source should parse");
        assert_eq!(
            document
                .edges
                .first()
                .and_then(|edge| edge.link_style.clone()),
            None
        );
        let emitted = assert_stable(source);
        assert!(
            emitted
                .ends_with("  e1@{ curve: basis }\n  linkStyle 0 interpolate linear stroke:#f00"),
            "{emitted}"
        );

        let without_metadata = parse_flowchart(
            "flowchart TD\n  A --> B\n  linkStyle 0 interpolate linear stroke:#f00",
        )
        .expect("source should parse");
        assert_eq!(
            without_metadata
                .edges
                .first()
                .and_then(|edge| edge.link_style.clone()),
            Some("interpolate linear stroke:#f00".to_string())
        );
    }

    /// Text in an edge label written between the dashes is not blanked out
    /// like the other labels, so an `@` in it has to be told apart from an
    /// edge id by what follows it.
    #[test]
    fn an_at_in_an_unquoted_edge_label_is_text() {
        for (source, label) in [
            (
                "flowchart TD\n  A -- mail@example.com --> B",
                "mail@example.com",
            ),
            ("flowchart TD\n  A -- me@x --> B", "me@x"),
            ("flowchart TD\n  A -- me@outlook --> B", "me@outlook"),
        ] {
            let document = parse_flowchart(source)
                .unwrap_or_else(|error| panic!("`{source}` should open: {error}"));
            let edge = document.edges.first().expect("the diagram has an edge");
            assert_eq!(edge.label.as_deref(), Some(label), "for {source}");
            assert_eq!(edge.user_id, None, "for {source}");
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn an_edge_id_shared_with_a_node_or_a_group_is_refused() {
        for source in [
            "flowchart TD\n  A e1@--> B\n  e1[ノード]",
            "flowchart TD\n  A g@--> B\n  subgraph g [まとまり]\n    C\n  end",
            // A node declared through `&` is a node all the same.
            "flowchart TD\n  A & e1 --> B\n  C e1@--> D",
            "flowchart TD\n  A --> B & e1\n  C e1@--> D",
        ] {
            let error = parse_flowchart(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(
                format!("{error}").contains("テキストのまま編集してください"),
                "unexpected message for `{source}`: {error}"
            );
        }
    }

    /// A statement after `;` on a kept line would be written back twice: once
    /// verbatim and once from the model.
    #[test]
    fn a_kept_line_that_carries_another_statement_is_refused() {
        let error = parse_flowchart("flowchart TD\n  A --> B\n  style A fill:#f00; A --> C")
            .err()
            .expect("the chained edge would be written twice");
        assert!(format!("{error}").contains("テキストのまま編集してください"));

        for source in [
            "flowchart TD\n  A --> B\n  B --> C\n  linkStyle 0 stroke:#f00; linkStyle 1 stroke:#00f",
            "---\ntitle: a; b\n---\nflowchart TD\n  A --> B",
            "flowchart TD\n  A --> B\n  click A \"https://example.com/?a=1;b=2\"",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn style_text_that_would_break_the_statement_is_refused() {
        for text in [
            "fill:#f96",
            "fill:#f96;",
            "stroke:#f00,stroke-width:2px",
            "font-family:'A B'",
            "",
        ] {
            assert!(is_valid_style_text(text), "`{text}` should be accepted");
        }
        for text in [
            "fill:#f96; A --> Z",
            "fill:#f96;;",
            "fill:#f96 %% memo",
            "font-family:'A",
            "a\nb",
            "fill:#f96 {",
            "@{ ",
        ] {
            assert!(!is_valid_style_text(text), "`{text}` should be refused");
        }
    }

    /// A kept line ending with `{` is read up to the next `}` line. Carried,
    /// such a statement would be written before the kept lines and pull them
    /// into its block, so it stays where it was written.
    #[test]
    fn a_statement_ending_with_a_brace_is_not_carried() {
        for source in [
            "flowchart TD\n  A --> B\n  B --> C\n  style A fill:#0f0\n  linkStyle 0 stroke:#f00 {",
            "flowchart TD\n  A --> B\n  style A fill:#0f0\n  classDef c fill:#f00 {",
        ] {
            let document = parse_flowchart(source).expect("source should parse");
            assert!(document.class_defs.is_empty(), "for {source}");
            assert!(
                document.edges.iter().all(|edge| edge.link_style.is_none()),
                "for {source}"
            );
            let emitted = assert_stable(source);
            assert!(
                emitted.ends_with("  style A fill:#0f0\n  linkStyle 0 stroke:#f00 {")
                    || emitted.ends_with("  style A fill:#0f0\n  classDef c fill:#f00 {"),
                "the kept lines should keep their order:\n{emitted}"
            );
        }
    }

    #[test]
    fn unused_id_skips_edge_ids() {
        let document =
            parse_flowchart("flowchart TD\n  A node@--> B").expect("source should parse");
        assert_eq!(document.unused_id("node"), "node1");
    }

    /// `merman` cuts a `%%{ … }%%` out of its line and reads what is left; kept
    /// as a comment, the statement after it would be drawn but out of the form's
    /// reach — and written a second time by the model, which reads it too.
    #[test]
    fn statements_merman_reads_around_a_directive_are_refused() {
        crate::mermaid_model::preprocessing_cases::assert_directive_rules(
            "flowchart TD",
            "A --> B",
            "A %%{init: {}}%% --> B",
        );
    }

    /// The model holds labels decoded, so a `#…;` or a `%%{` that came out of
    /// an entity has to be encoded again, or the next reading would decode it
    /// once more or cut the rest of the diagram out.
    #[test]
    fn labels_that_preprocessing_would_rewrite_are_written_encoded() {
        let source = "%%{init: {}}%%\nflowchart TD\n    A[\"#35;quot; と #37;#37;{x\"] -->|#35;amp;| B[\"a %%{init: {}}%% b\"]";
        assert!(parse_flowchart(source).is_err(), "{source}");

        let source =
            "%%{init: {}}%%\nflowchart TD\n    A[\"#35;quot; と #37;#37;{x\"] -->|#35;amp;| B";
        let document = parse_flowchart(source).expect("the labels should open");
        assert_eq!(document.nodes[0].label, "#quot; と %%{x");
        assert_eq!(document.edges[0].label.as_deref(), Some("#amp;"));
        let mut document = document;
        document.nodes[1].label = "c %%{init: {}}%% d #lt;".to_string();
        let whole = crate::mermaid_model::MermaidDocument::Flowchart(document.clone());
        assert_eq!(whole.structure_error(), None);
        let emitted = emit_flowchart(&document);
        assert_renders(&emitted);
        let again = parse_flowchart(&emitted).expect("the emitted diagram should reopen");
        assert_eq!(again.nodes, document.nodes);
        assert_eq!(again.edges[0].label, document.edges[0].label);
        assert_eq!(emit_flowchart(&again), emitted);
    }
}
