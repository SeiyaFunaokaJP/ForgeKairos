//! The entry point of the structured Mermaid editors: which kind of diagram a
//! fence holds, and the document types the editor edits.
//!
//! Every diagram kind has a module of its own ([`crate::flowchart_model`],
//! [`crate::pie_model`], [`crate::state_model`], [`crate::sequence_model`],
//! [`crate::er_model`], [`crate::class_model`], [`crate::gantt_model`],
//! [`crate::mindmap_model`], [`crate::timeline_model`],
//! [`crate::gitgraph_model`], [`crate::journey_model`],
//! [`crate::quadrant_model`], [`crate::xychart_model`], [`crate::radar_model`],
//! [`crate::requirement_model`], [`crate::c4_model`], [`crate::block_model`],
//! [`crate::sankey_model`], [`crate::packet_model`], [`crate::venn_model`],
//! [`crate::kanban_model`], [`crate::treemap_model`],
//! [`crate::architecture_model`], [`crate::tree_view_model`],
//! [`crate::ishikawa_model`], [`crate::event_modeling_model`]) with the
//! same three functions — `is_<kind>_source`, `parse_<kind>` and `emit_<kind>` —
//! and they all keep the same two promises:
//!
//! - Emitting is idempotent from its second application, so opening and
//!   confirming the editor twice does not keep rewriting the fence.
//! - Nothing disappears. A construct the model does not carry is either held
//!   verbatim in a preserved line, or the parse fails with a reason in
//!   Japanese, which the editor shows instead of silently dropping the
//!   construct.
//!
//! The scanning helpers that all the kinds need live here, because the parts of
//! a fence that come before the diagram keyword — YAML frontmatter, `%%{init}%%`
//! directives and comments — look the same whatever the diagram is.

use anyhow::{Result, bail};

use crate::architecture_model::{
    ArchitectureDocument, emit_architecture, is_architecture_source, parse_architecture,
};
use crate::block_model::{BlockDocument, emit_block, is_block_source, parse_block};
use crate::c4_model::{C4Document, emit_c4, is_c4_source, parse_c4};
use crate::class_model::{ClassDocument, emit_class, is_class_source, parse_class};
use crate::er_model::{ErDocument, emit_er, is_er_source, parse_er};
use crate::event_modeling_model::{
    EventModelingDocument, emit_event_modeling, is_event_modeling_source, parse_event_modeling,
};
use crate::flowchart_model::{
    FlowchartDocument, PreservedLine, PreservedPosition, emit_flowchart, is_flowchart_source,
    parse_flowchart,
};
use crate::gantt_model::{GanttDocument, emit_gantt, is_gantt_source, parse_gantt};
use crate::gitgraph_model::{GitGraphDocument, emit_gitgraph, is_gitgraph_source, parse_gitgraph};
use crate::ishikawa_model::{IshikawaDocument, emit_ishikawa, is_ishikawa_source, parse_ishikawa};
use crate::journey_model::{JourneyDocument, emit_journey, is_journey_source, parse_journey};
use crate::kanban_model::{KanbanDocument, emit_kanban, is_kanban_source, parse_kanban};
use crate::mindmap_model::{MindmapDocument, emit_mindmap, is_mindmap_source, parse_mindmap};
use crate::packet_model::{PacketDocument, emit_packet, is_packet_source, parse_packet};
use crate::pie_model::{PieDocument, emit_pie, is_pie_source, parse_pie};
use crate::quadrant_model::{QuadrantDocument, emit_quadrant, is_quadrant_source, parse_quadrant};
use crate::radar_model::{RadarDocument, emit_radar, is_radar_source, parse_radar};
use crate::requirement_model::{
    RequirementDocument, emit_requirement, is_requirement_source, parse_requirement,
};
use crate::sankey_model::{SankeyDocument, emit_sankey, is_sankey_source, parse_sankey};
use crate::sequence_model::{SequenceDocument, emit_sequence, is_sequence_source, parse_sequence};
use crate::state_model::{StateDocument, emit_state, is_state_source, parse_state};
use crate::timeline_model::{TimelineDocument, emit_timeline, is_timeline_source, parse_timeline};
use crate::tree_view_model::{
    TreeViewDocument, emit_tree_view, is_tree_view_source, parse_tree_view,
};
use crate::treemap_model::{TreemapDocument, emit_treemap, is_treemap_source, parse_treemap};
use crate::venn_model::{VennDocument, emit_venn, is_venn_source, parse_venn};
use crate::xychart_model::{XyChartDocument, emit_xychart, is_xychart_source, parse_xychart};

/// The indentation the emitters write statements with. Mermaid ignores it; it
/// is here so that a fence the editor wrote looks like one a person wrote.
pub(crate) const INDENT: &str = "    ";

/// The diagram kinds the structured editor knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidDiagramKind {
    Flowchart,
    Pie,
    State,
    Sequence,
    Er,
    Class,
    Gantt,
    Mindmap,
    Timeline,
    GitGraph,
    Journey,
    QuadrantChart,
    XyChart,
    Radar,
    Requirement,
    C4,
    Block,
    Sankey,
    Packet,
    Venn,
    Kanban,
    Treemap,
    Architecture,
    TreeView,
    Ishikawa,
    EventModeling,
}

impl MermaidDiagramKind {
    /// Every kind, in the order the editor's menus list them.
    pub const ALL: [MermaidDiagramKind; 26] = [
        MermaidDiagramKind::Flowchart,
        MermaidDiagramKind::Pie,
        MermaidDiagramKind::State,
        MermaidDiagramKind::Sequence,
        MermaidDiagramKind::Er,
        MermaidDiagramKind::Class,
        MermaidDiagramKind::Gantt,
        MermaidDiagramKind::Mindmap,
        MermaidDiagramKind::Timeline,
        MermaidDiagramKind::GitGraph,
        MermaidDiagramKind::Journey,
        MermaidDiagramKind::QuadrantChart,
        MermaidDiagramKind::XyChart,
        MermaidDiagramKind::Radar,
        MermaidDiagramKind::Requirement,
        MermaidDiagramKind::C4,
        MermaidDiagramKind::Block,
        MermaidDiagramKind::Sankey,
        MermaidDiagramKind::Packet,
        MermaidDiagramKind::Venn,
        MermaidDiagramKind::Kanban,
        MermaidDiagramKind::Treemap,
        MermaidDiagramKind::Architecture,
        MermaidDiagramKind::TreeView,
        MermaidDiagramKind::Ishikawa,
        MermaidDiagramKind::EventModeling,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Flowchart => i18n::tc!("Mermaid diagram kind", "Flowchart"),
            Self::Pie => i18n::tc!("Mermaid diagram kind", "Pie Chart"),
            Self::State => i18n::tc!("Mermaid diagram kind", "State Diagram"),
            Self::Sequence => i18n::tc!("Mermaid diagram kind", "Sequence Diagram"),
            Self::Er => i18n::tc!("Mermaid diagram kind", "ER Diagram"),
            Self::Class => i18n::tc!("Mermaid diagram kind", "Class Diagram"),
            Self::Gantt => i18n::tc!("Mermaid diagram kind", "Gantt Chart"),
            Self::Mindmap => i18n::tc!("Mermaid diagram kind", "Mindmap"),
            Self::Timeline => i18n::tc!("Mermaid diagram kind", "Timeline"),
            Self::GitGraph => i18n::tc!("Mermaid diagram kind", "Git Graph"),
            Self::Journey => i18n::tc!("Mermaid diagram kind", "User Journey"),
            Self::QuadrantChart => i18n::tc!("Mermaid diagram kind", "Quadrant Chart"),
            Self::XyChart => i18n::tc!("Mermaid diagram kind", "XY Chart (Bar/Line)"),
            Self::Radar => i18n::tc!("Mermaid diagram kind", "Radar Chart"),
            Self::Requirement => i18n::tc!("Mermaid diagram kind", "Requirement Diagram"),
            Self::C4 => i18n::tc!("Mermaid diagram kind", "C4 Diagram"),
            Self::Block => i18n::tc!("Mermaid diagram kind", "Block Diagram"),
            Self::Sankey => i18n::tc!("Mermaid diagram kind", "Sankey Diagram"),
            Self::Packet => i18n::tc!("Mermaid diagram kind", "Packet Diagram"),
            Self::Venn => i18n::tc!("Mermaid diagram kind", "Venn Diagram"),
            Self::Kanban => i18n::tc!("Mermaid diagram kind", "Kanban"),
            Self::Treemap => i18n::tc!("Mermaid diagram kind", "Treemap"),
            Self::Architecture => i18n::tc!("Mermaid diagram kind", "Architecture Diagram"),
            Self::TreeView => i18n::tc!("Mermaid diagram kind", "Tree View"),
            Self::Ishikawa => i18n::tc!("Mermaid diagram kind", "Ishikawa Diagram"),
            Self::EventModeling => i18n::tc!("Mermaid diagram kind", "Event Modeling"),
        }
    }
}

/// An editable Mermaid diagram.
#[derive(Debug, Clone, PartialEq)]
pub enum MermaidDocument {
    Flowchart(FlowchartDocument),
    Pie(PieDocument),
    State(StateDocument),
    Sequence(SequenceDocument),
    Er(ErDocument),
    Class(ClassDocument),
    Gantt(GanttDocument),
    Mindmap(MindmapDocument),
    Timeline(TimelineDocument),
    GitGraph(GitGraphDocument),
    Journey(JourneyDocument),
    QuadrantChart(QuadrantDocument),
    XyChart(XyChartDocument),
    Radar(RadarDocument),
    Requirement(RequirementDocument),
    C4(C4Document),
    Block(BlockDocument),
    Sankey(SankeyDocument),
    Packet(PacketDocument),
    Venn(VennDocument),
    Kanban(KanbanDocument),
    Treemap(TreemapDocument),
    Architecture(ArchitectureDocument),
    TreeView(TreeViewDocument),
    Ishikawa(IshikawaDocument),
    EventModeling(EventModelingDocument),
}

impl MermaidDocument {
    /// A blank document of `kind`, used when the editor is opened on an empty
    /// fence.
    pub fn empty(kind: MermaidDiagramKind) -> Self {
        match kind {
            MermaidDiagramKind::Flowchart => Self::Flowchart(FlowchartDocument::empty()),
            MermaidDiagramKind::Pie => Self::Pie(PieDocument::empty()),
            MermaidDiagramKind::State => Self::State(StateDocument::empty()),
            MermaidDiagramKind::Sequence => Self::Sequence(SequenceDocument::empty()),
            MermaidDiagramKind::Er => Self::Er(ErDocument::empty()),
            MermaidDiagramKind::Class => Self::Class(ClassDocument::empty()),
            MermaidDiagramKind::Gantt => Self::Gantt(GanttDocument::empty()),
            MermaidDiagramKind::Mindmap => Self::Mindmap(MindmapDocument::empty()),
            MermaidDiagramKind::Timeline => Self::Timeline(TimelineDocument::empty()),
            MermaidDiagramKind::GitGraph => Self::GitGraph(GitGraphDocument::empty()),
            MermaidDiagramKind::Journey => Self::Journey(JourneyDocument::empty()),
            MermaidDiagramKind::QuadrantChart => Self::QuadrantChart(QuadrantDocument::empty()),
            MermaidDiagramKind::XyChart => Self::XyChart(XyChartDocument::empty()),
            MermaidDiagramKind::Radar => Self::Radar(RadarDocument::empty()),
            MermaidDiagramKind::Requirement => Self::Requirement(RequirementDocument::empty()),
            MermaidDiagramKind::C4 => Self::C4(C4Document::empty()),
            MermaidDiagramKind::Block => Self::Block(BlockDocument::empty()),
            MermaidDiagramKind::Sankey => Self::Sankey(SankeyDocument::empty()),
            MermaidDiagramKind::Packet => Self::Packet(PacketDocument::empty()),
            MermaidDiagramKind::Venn => Self::Venn(VennDocument::empty()),
            MermaidDiagramKind::Kanban => Self::Kanban(KanbanDocument::empty()),
            MermaidDiagramKind::Treemap => Self::Treemap(TreemapDocument::empty()),
            MermaidDiagramKind::Architecture => Self::Architecture(ArchitectureDocument::empty()),
            MermaidDiagramKind::TreeView => Self::TreeView(TreeViewDocument::empty()),
            MermaidDiagramKind::Ishikawa => Self::Ishikawa(IshikawaDocument::empty()),
            MermaidDiagramKind::EventModeling => {
                Self::EventModeling(EventModelingDocument::empty())
            }
        }
    }

    pub fn kind(&self) -> MermaidDiagramKind {
        match self {
            Self::Flowchart(_) => MermaidDiagramKind::Flowchart,
            Self::Pie(_) => MermaidDiagramKind::Pie,
            Self::State(_) => MermaidDiagramKind::State,
            Self::Sequence(_) => MermaidDiagramKind::Sequence,
            Self::Er(_) => MermaidDiagramKind::Er,
            Self::Class(_) => MermaidDiagramKind::Class,
            Self::Gantt(_) => MermaidDiagramKind::Gantt,
            Self::Mindmap(_) => MermaidDiagramKind::Mindmap,
            Self::Timeline(_) => MermaidDiagramKind::Timeline,
            Self::GitGraph(_) => MermaidDiagramKind::GitGraph,
            Self::Journey(_) => MermaidDiagramKind::Journey,
            Self::QuadrantChart(_) => MermaidDiagramKind::QuadrantChart,
            Self::XyChart(_) => MermaidDiagramKind::XyChart,
            Self::Radar(_) => MermaidDiagramKind::Radar,
            Self::Requirement(_) => MermaidDiagramKind::Requirement,
            Self::C4(_) => MermaidDiagramKind::C4,
            Self::Block(_) => MermaidDiagramKind::Block,
            Self::Sankey(_) => MermaidDiagramKind::Sankey,
            Self::Packet(_) => MermaidDiagramKind::Packet,
            Self::Venn(_) => MermaidDiagramKind::Venn,
            Self::Kanban(_) => MermaidDiagramKind::Kanban,
            Self::Treemap(_) => MermaidDiagramKind::Treemap,
            Self::Architecture(_) => MermaidDiagramKind::Architecture,
            Self::TreeView(_) => MermaidDiagramKind::TreeView,
            Self::Ishikawa(_) => MermaidDiagramKind::Ishikawa,
            Self::EventModeling(_) => MermaidDiagramKind::EventModeling,
        }
    }

    /// Writes the document back as Mermaid source, without a trailing newline.
    pub fn emit(&self) -> String {
        match self {
            Self::Flowchart(document) => emit_flowchart(document),
            Self::Pie(document) => emit_pie(document),
            Self::State(document) => emit_state(document),
            Self::Sequence(document) => emit_sequence(document),
            Self::Er(document) => emit_er(document),
            Self::Class(document) => emit_class(document),
            Self::Gantt(document) => emit_gantt(document),
            Self::Mindmap(document) => emit_mindmap(document),
            Self::Timeline(document) => emit_timeline(document),
            Self::GitGraph(document) => emit_gitgraph(document),
            Self::Journey(document) => emit_journey(document),
            Self::QuadrantChart(document) => emit_quadrant(document),
            Self::XyChart(document) => emit_xychart(document),
            Self::Radar(document) => emit_radar(document),
            Self::Requirement(document) => emit_requirement(document),
            Self::C4(document) => emit_c4(document),
            Self::Block(document) => emit_block(document),
            Self::Sankey(document) => emit_sankey(document),
            Self::Packet(document) => emit_packet(document),
            Self::Venn(document) => emit_venn(document),
            Self::Kanban(document) => emit_kanban(document),
            Self::Treemap(document) => emit_treemap(document),
            Self::Architecture(document) => emit_architecture(document),
            Self::TreeView(document) => emit_tree_view(document),
            Self::Ishikawa(document) => emit_ishikawa(document),
            Self::EventModeling(document) => emit_event_modeling(document),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    ///
    /// Only the kinds whose items refer to each other across the whole
    /// document — a message names a participant, a relationship names an
    /// entity, a task starts after another task — can be brought into a state
    /// that emitting would spoil; the others are checked field by field by the
    /// editor. For the kinds that scan their source line by line, the emitted
    /// text is also held to [`preprocessing_problem`].
    pub fn structure_error(&self) -> Option<String> {
        let kind_error = match self {
            Self::Sequence(document) => document.structure_error(),
            Self::Er(document) => document.structure_error(),
            Self::Gantt(document) => document.structure_error(),
            Self::Mindmap(document) => document.structure_error(),
            Self::Timeline(document) => document.structure_error(),
            Self::GitGraph(document) => document.structure_error(),
            Self::Journey(document) => document.structure_error(),
            Self::QuadrantChart(document) => document.structure_error(),
            Self::XyChart(document) => document.structure_error(),
            Self::Radar(document) => document.structure_error(),
            Self::Requirement(document) => document.structure_error(),
            Self::C4(document) => document.structure_error(),
            Self::Block(document) => document.structure_error(),
            Self::Sankey(document) => document.structure_error(),
            Self::Packet(document) => document.structure_error(),
            Self::Venn(document) => document.structure_error(),
            Self::Kanban(document) => document.structure_error(),
            Self::Treemap(document) => document.structure_error(),
            Self::Architecture(document) => document.structure_error(),
            Self::TreeView(document) => document.structure_error(),
            Self::Ishikawa(document) => document.structure_error(),
            Self::EventModeling(document) => document.structure_error(),
            Self::Flowchart(_) | Self::Pie(_) | Self::State(_) | Self::Class(_) => None,
        };
        if kind_error.is_some() {
            return kind_error;
        }
        self.preprocessing_error()
    }

    /// What a field holds is checked on its own by the editor, but only the
    /// whole text shows where `merman`'s preprocessing would join two
    /// statements or move a quote from one field to another.
    fn preprocessing_error(&self) -> Option<String> {
        match self.kind() {
            MermaidDiagramKind::Flowchart
            | MermaidDiagramKind::Pie
            | MermaidDiagramKind::State
            | MermaidDiagramKind::Sequence
            | MermaidDiagramKind::Er
            | MermaidDiagramKind::Class
            | MermaidDiagramKind::Gantt
            | MermaidDiagramKind::Mindmap
            | MermaidDiagramKind::Timeline
            | MermaidDiagramKind::GitGraph => preprocessing_problem(&self.emit()),
            _ => None,
        }
    }
}

/// Which kind of diagram `source` is, judged by its first statement, or `None`
/// when it is a kind the structured editor does not know — including an empty
/// fence, for which the caller picks the kind itself.
///
/// Cheap enough to call before parsing.
pub fn detect_kind(source: &str) -> Option<MermaidDiagramKind> {
    if is_flowchart_source(source) {
        return Some(MermaidDiagramKind::Flowchart);
    }
    if is_pie_source(source) {
        return Some(MermaidDiagramKind::Pie);
    }
    if is_state_source(source) {
        return Some(MermaidDiagramKind::State);
    }
    if is_sequence_source(source) {
        return Some(MermaidDiagramKind::Sequence);
    }
    if is_er_source(source) {
        return Some(MermaidDiagramKind::Er);
    }
    if is_class_source(source) {
        return Some(MermaidDiagramKind::Class);
    }
    if is_gantt_source(source) {
        return Some(MermaidDiagramKind::Gantt);
    }
    if is_mindmap_source(source) {
        return Some(MermaidDiagramKind::Mindmap);
    }
    if is_timeline_source(source) {
        return Some(MermaidDiagramKind::Timeline);
    }
    if is_gitgraph_source(source) {
        return Some(MermaidDiagramKind::GitGraph);
    }
    if is_journey_source(source) {
        return Some(MermaidDiagramKind::Journey);
    }
    if is_quadrant_source(source) {
        return Some(MermaidDiagramKind::QuadrantChart);
    }
    if is_xychart_source(source) {
        return Some(MermaidDiagramKind::XyChart);
    }
    if is_radar_source(source) {
        return Some(MermaidDiagramKind::Radar);
    }
    if is_requirement_source(source) {
        return Some(MermaidDiagramKind::Requirement);
    }
    if is_c4_source(source) {
        return Some(MermaidDiagramKind::C4);
    }
    if is_block_source(source) {
        return Some(MermaidDiagramKind::Block);
    }
    if is_sankey_source(source) {
        return Some(MermaidDiagramKind::Sankey);
    }
    if is_packet_source(source) {
        return Some(MermaidDiagramKind::Packet);
    }
    if is_venn_source(source) {
        return Some(MermaidDiagramKind::Venn);
    }
    if is_kanban_source(source) {
        return Some(MermaidDiagramKind::Kanban);
    }
    if is_treemap_source(source) {
        return Some(MermaidDiagramKind::Treemap);
    }
    if is_architecture_source(source) {
        return Some(MermaidDiagramKind::Architecture);
    }
    if is_tree_view_source(source) {
        return Some(MermaidDiagramKind::TreeView);
    }
    if is_ishikawa_source(source) {
        return Some(MermaidDiagramKind::Ishikawa);
    }
    if is_event_modeling_source(source) {
        return Some(MermaidDiagramKind::EventModeling);
    }
    None
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_document(source: &str) -> Result<MermaidDocument> {
    let document = parse_document_of_its_kind(source)?;
    // Some kinds take their labels from what `merman` read after its
    // preprocessing but carry other lines as written: a document that opened
    // with such a line would never pass `structure_error`, so it does not open.
    if let Some(problem) = document.preprocessing_error() {
        bail!("{problem}");
    }
    Ok(document)
}

fn parse_document_of_its_kind(source: &str) -> Result<MermaidDocument> {
    match detect_kind(source) {
        Some(MermaidDiagramKind::Flowchart) => {
            parse_flowchart(source).map(MermaidDocument::Flowchart)
        }
        Some(MermaidDiagramKind::Pie) => parse_pie(source).map(MermaidDocument::Pie),
        Some(MermaidDiagramKind::State) => parse_state(source).map(MermaidDocument::State),
        Some(MermaidDiagramKind::Sequence) => parse_sequence(source).map(MermaidDocument::Sequence),
        Some(MermaidDiagramKind::Er) => parse_er(source).map(MermaidDocument::Er),
        Some(MermaidDiagramKind::Class) => parse_class(source).map(MermaidDocument::Class),
        Some(MermaidDiagramKind::Gantt) => parse_gantt(source).map(MermaidDocument::Gantt),
        Some(MermaidDiagramKind::Mindmap) => parse_mindmap(source).map(MermaidDocument::Mindmap),
        Some(MermaidDiagramKind::Timeline) => parse_timeline(source).map(MermaidDocument::Timeline),
        Some(MermaidDiagramKind::GitGraph) => parse_gitgraph(source).map(MermaidDocument::GitGraph),
        Some(MermaidDiagramKind::Journey) => parse_journey(source).map(MermaidDocument::Journey),
        Some(MermaidDiagramKind::QuadrantChart) => {
            parse_quadrant(source).map(MermaidDocument::QuadrantChart)
        }
        Some(MermaidDiagramKind::XyChart) => parse_xychart(source).map(MermaidDocument::XyChart),
        Some(MermaidDiagramKind::Radar) => parse_radar(source).map(MermaidDocument::Radar),
        Some(MermaidDiagramKind::Requirement) => {
            parse_requirement(source).map(MermaidDocument::Requirement)
        }
        Some(MermaidDiagramKind::C4) => parse_c4(source).map(MermaidDocument::C4),
        Some(MermaidDiagramKind::Block) => parse_block(source).map(MermaidDocument::Block),
        Some(MermaidDiagramKind::Sankey) => parse_sankey(source).map(MermaidDocument::Sankey),
        Some(MermaidDiagramKind::Packet) => parse_packet(source).map(MermaidDocument::Packet),
        Some(MermaidDiagramKind::Venn) => parse_venn(source).map(MermaidDocument::Venn),
        Some(MermaidDiagramKind::Kanban) => parse_kanban(source).map(MermaidDocument::Kanban),
        Some(MermaidDiagramKind::Treemap) => parse_treemap(source).map(MermaidDocument::Treemap),
        Some(MermaidDiagramKind::Architecture) => {
            parse_architecture(source).map(MermaidDocument::Architecture)
        }
        Some(MermaidDiagramKind::TreeView) => {
            parse_tree_view(source).map(MermaidDocument::TreeView)
        }
        Some(MermaidDiagramKind::Ishikawa) => parse_ishikawa(source).map(MermaidDocument::Ishikawa),
        Some(MermaidDiagramKind::EventModeling) => {
            parse_event_modeling(source).map(MermaidDocument::EventModeling)
        }
        None => {
            bail!(i18n::t!(
                "The structured editor does not support this diagram. Edit it as text."
            ))
        }
    }
}

/// The first line that says which diagram this is: the first non-empty line
/// that is not YAML frontmatter, a `%%{init}%%` directive or a comment.
pub(crate) fn first_statement_line(source: &str) -> Option<&str> {
    let mut lines = source.lines().map(str::trim).peekable();
    let mut in_frontmatter = false;
    let mut seen_content = false;
    let mut in_directive = false;

    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if in_directive {
            if line.contains("}%%") {
                in_directive = false;
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
                in_directive = true;
            }
            continue;
        }
        return Some(line);
    }
    None
}

/// What the leading lines of a fence hold: everything before the diagram
/// keyword, and where the keyword is.
pub(crate) struct Preamble {
    pub preserved: Vec<PreservedLine>,
    /// Index into the scanned lines of the diagram keyword, or `None` for a
    /// fence that holds nothing but frontmatter, directives and comments.
    pub header: Option<usize>,
}

/// Reads the frontmatter, `%%{init}%%` directives and comments that come before
/// the diagram keyword. They have to stay in front of it, so they are all
/// preserved at [`PreservedPosition::Preamble`].
pub(crate) fn scan_preamble(lines: &[&str]) -> Preamble {
    let mut preserved = Vec::new();
    let mut index = 0;
    let mut seen_content = false;

    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        let start = index;
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        if !seen_content && trimmed == "---" {
            seen_content = true;
            index += 1;
            while lines.get(index).is_some_and(|line| line.trim() != "---") {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        seen_content = true;
        if trimmed.starts_with("%%") {
            index += 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        return Preamble {
            preserved,
            header: Some(index),
        };
    }

    Preamble {
        preserved,
        header: None,
    }
}

/// What follows `keyword` at the start of `line`, with its leading whitespace
/// removed, or `None` when the line does not start with that word.
///
/// The word has to end where Mermaid's lexers end it: at whitespace, at the
/// statement separator or at the end of the line. Without that check `pieces`
/// would read as a `pie` chart.
pub(crate) fn keyword_tail<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if rest.is_empty() || rest.starts_with(|character: char| character.is_whitespace()) {
        return Some(rest.trim_start());
    }
    None
}

/// [`keyword_tail`] for the diagram keywords `merman` takes in any case, such as
/// `ishikawa`.
pub(crate) fn keyword_tail_ignoring_case<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let prefix = line.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = line.get(keyword.len()..)?;
    if rest.is_empty() || rest.starts_with(|character: char| character.is_whitespace()) {
        return Some(rest.trim_start());
    }
    None
}

/// [`keyword_tail`] for the keywords whose lexer ignores case and lets the word
/// end at a delimiter as well — `accDescr{`, `classDef:` and their like.
pub(crate) fn statement_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let prefix = line.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = line.get(keyword.len()..)?;
    if rest.is_empty()
        || rest.starts_with(|character: char| {
            character.is_whitespace()
                || matches!(character, ':' | '{' | '}' | '[' | ']' | '"' | ';')
        })
    {
        return Some(rest.trim_start());
    }
    None
}

/// Joins source lines into one preserved entry, keeping their original
/// indentation and dropping only the whitespace at their ends.
pub(crate) fn join_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

const DIRECTIVE_START: &str = "%%{";
const DIRECTIVE_END: &str = "}%%";

/// Why `merman` would read `source` as something other than what the
/// structured editors of the first ten kinds read, or `None` when both read it
/// the same way.
///
/// `merman` rewrites the whole text before any diagram parser sees a line of
/// it, while those editors scan the lines as they are written. See
/// [`directive_problem`] and [`html_attribute_problem`].
pub(crate) fn preprocessing_problem(source: &str) -> Option<String> {
    directive_problem(source).or_else(|| html_attribute_problem(source, false))
}

/// [`preprocessing_problem`] for the kinds that take their labels from what
/// `merman` parsed: a `="…"` that closes a quoted label on its own line is
/// read by them just as `merman` reads it, and written back with the `'` it
/// was read with. That rewrite is what lets `A["<b class="x">太字</b>"]`
/// work at all, so refusing it would refuse an ordinary label.
pub(crate) fn preprocessing_problem_for_parsed_labels(source: &str) -> Option<String> {
    directive_problem(source).or_else(|| html_attribute_problem(source, true))
}

/// Whether `value`, written into a field, would start a `%%{ … }%%` directive
/// that `merman` cuts out of the text wherever it stands, quotes or not.
pub fn opens_directive(value: &str) -> bool {
    value.contains(DIRECTIVE_START)
}

/// `merman` cuts every `%%{ … }%%` out of the text, line breaks and quotes or
/// not, and drops everything after one that never closes. The editors hold a
/// directive as a line of its own, so one that shares its line with a
/// statement, sits inside a statement, or runs on over the lines after a
/// comment would be carried as a comment while `merman` reads what is around
/// it — and a rename or a deletion would not follow what it reads.
fn directive_problem(source: &str) -> Option<String> {
    let text = body_after_frontmatter(source);
    if !removes_directives(text) {
        return None;
    }
    let mut position = 0;
    while let Some(relative) = text
        .get(position..)
        .and_then(|rest| rest.find(DIRECTIVE_START))
    {
        let start = position + relative;
        let line_start = text
            .get(..start)
            .and_then(|before| before.rfind('\n'))
            .map_or(0, |newline| newline + 1);
        let line = text
            .get(line_start..)
            .and_then(|rest| rest.lines().next())
            .unwrap_or_default()
            .trim();
        let before = text
            .get(line_start.max(position)..start)
            .unwrap_or_default();
        let in_comment = before.trim_start().starts_with("%%");
        if !before.trim().is_empty() && !in_comment {
            return Some(i18n::t!(
                "There is a `%%{{` in the middle of a statement ({line}). Mermaid cuts `%%{{ … }}%%` out and joins what is before and after it, so the structured editor cannot handle it."
            ));
        }

        let after_start = start + DIRECTIVE_START.len();
        let Some(length) = text
            .get(after_start..)
            .and_then(|rest| rest.find(DIRECTIVE_END))
        else {
            let drops_a_statement = text
                .get(after_start..)
                .unwrap_or_default()
                .lines()
                .skip(1)
                .map(str::trim)
                .any(|line| !line.is_empty() && !line.starts_with("%%"));
            return drops_a_statement.then(|| {
                i18n::t!(
                    "There is a `%%{{` that is never closed ({line}). Mermaid does not read anything after it, so the structured editor cannot handle it."
                )
            });
        };
        let end = after_start + length + DIRECTIVE_END.len();
        if in_comment {
            if text
                .get(start..end)
                .is_some_and(|directive| directive.contains('\n'))
            {
                return Some(i18n::t!(
                    "A `%%{{` inside a comment runs on to the next line ({line}). Mermaid does not read the lines in between, so the structured editor cannot handle it."
                ));
            }
            position = end;
            continue;
        }

        let rest_of_line = text
            .get(end..)
            .and_then(|rest| rest.lines().next())
            .unwrap_or_default();
        let statement = rest_of_line
            .split(DIRECTIVE_START)
            .next()
            .unwrap_or_default()
            .trim();
        if !statement.is_empty() && !statement.starts_with("%%") {
            return Some(i18n::t!(
                "The structured editor cannot handle a statement written after `%%{{ … }}%%` on the same line ({statement}). Move the statement to the next line, then open the editor again."
            ));
        }
        position = end;
    }
    None
}

/// Whether `merman` cuts the directives out of `text` at all. 0.7 did so only
/// when it found a closed `%%{ … }%%` that opened with a word; 0.8 cuts every
/// `%%{`, and one that never closes runs to the end of the text, as the
/// optional closing marker of Mermaid's own directive pattern makes it do.
fn removes_directives(text: &str) -> bool {
    text.contains(DIRECTIVE_START)
}

/// What `merman` reads after it has taken the frontmatter off: only a `---`
/// at the very start opens one.
fn body_after_frontmatter(source: &str) -> &str {
    let Some(after_marker) = source.strip_prefix("---") else {
        return source;
    };
    let Some(open_line_end) = after_marker.find('\n') else {
        return source;
    };
    if !after_marker
        .get(..open_line_end)
        .is_some_and(|rest| rest.trim().is_empty())
    {
        return source;
    }
    let mut offset = "---".len() + open_line_end + 1;
    for line in source
        .get(offset..)
        .unwrap_or_default()
        .split_inclusive('\n')
    {
        offset += line.len();
        if line.trim() == "---" {
            return source.get(offset..).unwrap_or_default();
        }
    }
    source
}

/// `merman` turns every `="…"` between a `<tag` and the next `>` into `='…'`
/// before it reads a line, wherever the two stand. Inside one text that only
/// changes the quotes of an HTML attribute, but when the `<tag` and the `>` are
/// on different lines, or the `"` after the `=` closes a quoted field, it moves
/// where quoted fields open and close.
fn html_attribute_problem(source: &str, only_across_lines: bool) -> Option<String> {
    if !source.contains('<') || !source.contains("=\"") {
        return None;
    }
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = source.as_bytes();
    let mut probe = 0;
    while let Some(relative) = source.get(probe..).and_then(|rest| rest.find('<')) {
        let tag_start = probe + relative + 1;
        if !bytes.get(tag_start).copied().is_some_and(is_word) {
            probe = tag_start;
            continue;
        }
        let mut tag_end = tag_start + 1;
        while bytes.get(tag_end).copied().is_some_and(is_word) {
            tag_end += 1;
        }
        let Some(end) = source
            .get(tag_end..)
            .and_then(|rest| rest.find('>'))
            .map(|relative| tag_end + relative)
        else {
            probe = tag_start;
            continue;
        };
        let attributes = source.get(tag_end..end).unwrap_or_default();
        let mut cursor = 0;
        while let Some(relative) = attributes.get(cursor..).and_then(|rest| rest.find("=\"")) {
            let quote = cursor + relative + 1;
            let Some(closing) = attributes
                .get(quote + 1..)
                .and_then(|rest| rest.find('"'))
                .map(|relative| quote + 1 + relative)
            else {
                cursor = quote + 1;
                continue;
            };
            let quote_in_source = tag_end + quote;
            let line_start = source
                .get(..quote_in_source)
                .and_then(|before| before.rfind('\n'))
                .map_or(0, |newline| newline + 1);
            let closes_a_field = !only_across_lines
                && source
                    .get(line_start..quote_in_source)
                    .is_some_and(|before| !before.matches('"').count().is_multiple_of(2));
            if attributes.contains('\n') || closes_a_field {
                let line = source
                    .get(line_start..)
                    .and_then(|rest| rest.lines().next())
                    .unwrap_or_default()
                    .trim();
                return Some(i18n::t!(
                    "Mermaid reads the `\"` of `<tag attribute=\"…\">` as `'`, which changes which quotes pair up ({line}). Enclose the attribute in `'`."
                ));
            }
            cursor = closing + 1;
        }
        probe = end + 1;
    }
    None
}

/// Shows the bundled Japanese translations on the current thread until the
/// guard is dropped, for the tests that check a reason reaches the user
/// translated rather than as `merman`'s own English.
#[cfg(test)]
pub(crate) fn japanese_for_this_thread() -> i18n::test_support::LocaleOverride {
    i18n::test_support::LocaleOverride::new(Some(i18n_catalog::test_support::bundled_catalog("ja")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_an_english_and_a_japanese_label() {
        let english = MermaidDiagramKind::ALL.map(MermaidDiagramKind::label);
        for (kind, label) in MermaidDiagramKind::ALL.into_iter().zip(english) {
            assert!(
                !label.is_empty() && label.is_ascii(),
                "{kind:?} should have an English label, got {label}"
            );
        }

        let _japanese = japanese_for_this_thread();
        for (kind, english_label) in MermaidDiagramKind::ALL.into_iter().zip(english) {
            let label = kind.label();
            assert!(
                label.chars().any(|character| character > '\u{7f}'),
                "{kind:?} should have a Japanese label, got {label}"
            );
            assert_ne!(label, english_label, "{kind:?}");
        }
    }

    // The shape labels carry Mermaid's own brackets (`{ }`, `{{ }}`), which
    // `tr` must show exactly as written rather than read as placeholders.
    #[test]
    fn labels_in_japanese_read_as_they_did_before_the_catalog() {
        use crate::block_model::{BLOCK_LINKS, BlockItemKind, BlockShape};
        use crate::mindmap_model::MindmapShape;
        use crate::state_model::{NotePlacement, StateKind};

        let _japanese = japanese_for_this_thread();
        assert_eq!(
            MermaidDiagramKind::ALL.map(MermaidDiagramKind::label),
            [
                "フローチャート",
                "円グラフ",
                "状態遷移図",
                "シーケンス図",
                "ER 図",
                "クラス図",
                "ガントチャート",
                "マインドマップ",
                "タイムライン",
                "Git グラフ",
                "ユーザージャーニー",
                "4 象限チャート",
                "XY チャート (棒・折れ線)",
                "レーダーチャート",
                "要求図",
                "C4 図",
                "ブロック図",
                "サンキー図",
                "パケット図",
                "ベン図",
                "かんばん",
                "ツリーマップ",
                "アーキテクチャ図",
                "ツリービュー",
                "特性要因図",
                "イベントモデリング",
            ]
        );
        assert_eq!(
            BlockShape::ALL.map(BlockShape::label),
            [
                "四角 [ ]",
                "角丸 ( )",
                "競技場形 ([ ])",
                "二重枠 [[ ]]",
                "円柱 [( )]",
                "円 (( ))",
                "二重円 ((( )))",
                "旗形 > ]",
                "ひし形 { }",
                "六角形 {{ }}",
                "右傾き [/ /]",
                "左傾き [\\ \\]",
                "台形 [/ \\]",
                "逆台形 [\\ /]",
            ]
        );
        assert_eq!(
            MindmapShape::ALL.map(MindmapShape::label),
            [
                "標準",
                "四角 [ ]",
                "角丸 ( )",
                "円 (( ))",
                "爆発 )) ((",
                "雲 ) (",
                "六角形 {{ }}",
            ]
        );
        assert_eq!(
            BlockItemKind::ALL.map(BlockItemKind::label),
            ["ブロック", "矢印", "空き", "グループ"]
        );
        assert_eq!(
            BLOCK_LINKS.map(|(_, name)| i18n::tr(name)),
            ["矢印", "線だけ", "× 印", "○ 印"]
        );
        assert_eq!(
            StateKind::ALL.map(StateKind::label),
            [
                "通常の状態",
                "分岐 <<choice>>",
                "分流 <<fork>>",
                "合流 <<join>>"
            ]
        );
        assert_eq!(NotePlacement::ALL.map(NotePlacement::label), ["左", "右"]);
    }

    #[test]
    fn the_kind_is_detected_through_frontmatter_and_comments() {
        for (source, expected) in [
            (
                "flowchart TD\n  A --> B",
                Some(MermaidDiagramKind::Flowchart),
            ),
            ("graph LR\n  A --> B", Some(MermaidDiagramKind::Flowchart)),
            ("pie title x\n  \"a\" : 1", Some(MermaidDiagramKind::Pie)),
            (
                "stateDiagram-v2\n  [*] --> A",
                Some(MermaidDiagramKind::State),
            ),
            ("stateDiagram\n  [*] --> A", Some(MermaidDiagramKind::State)),
            (
                "sequenceDiagram\n  A->>B: hi",
                Some(MermaidDiagramKind::Sequence),
            ),
            ("erDiagram\n  A ||--o{ B : x", Some(MermaidDiagramKind::Er)),
            ("classDiagram\n  class A", Some(MermaidDiagramKind::Class)),
            (
                "gantt\n  A :a, 2026-01-01, 7d",
                Some(MermaidDiagramKind::Gantt),
            ),
            (
                "---\ntitle: t\n---\n%%{init: {\"theme\": \"dark\"}}%%\n%% メモ\npie\n  \"a\" : 1",
                Some(MermaidDiagramKind::Pie),
            ),
            ("", None),
            ("%% メモだけ", None),
            ("mindmap\n  root((中心))", Some(MermaidDiagramKind::Mindmap)),
            (
                "timeline\n    2024 : 企画",
                Some(MermaidDiagramKind::Timeline),
            ),
            ("journey\n  title x", Some(MermaidDiagramKind::Journey)),
            ("quadrantChart", Some(MermaidDiagramKind::QuadrantChart)),
            ("xychart-beta", Some(MermaidDiagramKind::XyChart)),
            ("radar-beta", Some(MermaidDiagramKind::Radar)),
            ("requirementDiagram", Some(MermaidDiagramKind::Requirement)),
            ("C4Deployment", Some(MermaidDiagramKind::C4)),
            ("block-beta", Some(MermaidDiagramKind::Block)),
            ("sankey-beta", Some(MermaidDiagramKind::Sankey)),
            ("packet-beta", Some(MermaidDiagramKind::Packet)),
            ("venn-beta", Some(MermaidDiagramKind::Venn)),
            ("kanban", Some(MermaidDiagramKind::Kanban)),
            ("treemap-beta", Some(MermaidDiagramKind::Treemap)),
            ("architecture-beta", Some(MermaidDiagramKind::Architecture)),
            ("treeView-beta", Some(MermaidDiagramKind::TreeView)),
            ("ishikawa-beta", Some(MermaidDiagramKind::Ishikawa)),
            ("eventmodeling", Some(MermaidDiagramKind::EventModeling)),
            ("zenuml\n  A->B: x", None),
            ("info", None),
        ] {
            assert_eq!(detect_kind(source), expected, "for {source}");
        }
    }

    #[test]
    fn a_document_round_trips_through_the_common_entry_point() {
        for source in [
            "flowchart TD\n    A[開始] --> B[終了]",
            "pie title 内訳\n    \"設計\" : 30\n    \"実装\" : 70",
            "stateDiagram-v2\n    [*] --> Idle\n    Idle --> [*]",
            "sequenceDiagram\n    participant A\n    participant B\n    A->>B: 依頼",
            "erDiagram\n    CUSTOMER {\n        string name\n    }\n    CUSTOMER ||--o{ ORDER : places",
            "classDiagram\n    class 動物 {\n        +鳴く() void\n    }\n    動物 <|-- 犬",
            "gantt\n    dateFormat YYYY-MM-DD\n    section 設計\n    要件定義 :req, 2026-01-06, 5d",
            "mindmap\n  root((中心))\n    枝[四角]\n    ::icon(fa fa-book)",
            "timeline\n    title 沿革\n    section 前半\n    2024 : 企画 : 設計",
        ] {
            let document = parse_document(source)
                .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
            let emitted = document.emit();
            let again = parse_document(&emitted)
                .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{emitted}"));
            assert_eq!(document.kind(), again.kind());
            assert_eq!(document, again, "round trip of {source} gave {emitted}");
            assert_eq!(emitted, again.emit());
        }
    }

    #[test]
    fn a_blank_document_is_available_for_every_kind() {
        for kind in MermaidDiagramKind::ALL {
            let document = MermaidDocument::empty(kind);
            assert_eq!(document.kind(), kind);
            assert_eq!(document.structure_error(), None, "{kind:?}");
            let emitted = document.emit();
            assert_eq!(
                detect_kind(&emitted),
                Some(kind),
                "a blank {kind:?} should emit source of its own kind, got {emitted}"
            );
            assert_eq!(
                parse_document(&emitted)
                    .unwrap_or_else(|error| panic!("a blank {kind:?} should parse: {error}")),
                document,
                "a blank {kind:?} should survive a round trip"
            );
        }
    }

    #[test]
    fn a_kind_with_no_model_at_all_is_refused() {
        for source in ["zenuml\n  A->B: x", "info"] {
            let error = parse_document(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(message.contains("structured editor"), "got: {message}");
        }
    }

    #[test]
    fn a_keyword_has_to_end_where_the_lexer_ends_it() {
        assert_eq!(keyword_tail("pie showData", "pie"), Some("showData"));
        assert_eq!(keyword_tail("pie", "pie"), Some(""));
        assert_eq!(keyword_tail("pieces", "pie"), None);
        assert_eq!(keyword_tail("日本語", "pie"), None);

        assert_eq!(statement_keyword("accDescr {", "accDescr"), Some("{"));
        assert_eq!(statement_keyword("accdescr: x", "accDescr"), Some(": x"));
        assert_eq!(statement_keyword("accDescription", "accDescr"), None);
        assert_eq!(statement_keyword("あ", "accDescr"), None);
    }

    #[test]
    fn directives_are_checked_the_way_merman_cuts_them() {
        for source in [
            "flowchart TD
  %%{init: {}}%% A --> B",
            "flowchart TD
  %%{init: {}}%%   %%{wrap}%% A --> B",
            "flowchart TD
  %%{init:
  {}}%% A --> B",
            "flowchart TD
  A %%{init: {}}%% --> B",
            "flowchart TD %%{init: {}}%%
  A --> B",
            "flowchart TD
  %%{init: {}}%%
  %% メモ %%{
  A --> B",
            "flowchart TD
  %% メモ %%{init:
  }%% A --> B",
            "---
title: 図
---
flowchart TD
  %%{init: {}}%% A --> B",
            // `merman` 0.7 cut nothing without a closed directive that opens
            // with a word; 0.8 cuts every `%%{`, and an unclosed one to the end.
            "flowchart TD
  %% メモ %%{
  A --> B",
            "flowchart TD
  %%{ }%% A --> B",
        ] {
            assert!(directive_problem(source).is_some(), "{source}");
        }
        for source in [
            "flowchart TD
  %%{init: {}}%%
  A --> B",
            "%%{init:
  {}}%%
flowchart TD
  A --> B",
            "flowchart TD
  %%{init: {}}%% %% メモ
  A --> B",
            "flowchart TD
  %%{init: {}}%%
  %% メモ %%{wrap}%% 続き
  A --> B",
            "flowchart TD
  %%{init: {}}%%
  A --> B
  %% 最後 %%{",
            "flowchart TD
  A --> B
  %% 最後 %%{",
            "flowchart TD
  A[\"100%\"] --> B",
            // A directive in the frontmatter is YAML, not a directive.
            "---
title: \"%%{init: {}}%% x\"
---
flowchart TD
  A --> B",
        ] {
            assert_eq!(directive_problem(source), None, "{source}");
        }
    }

    #[test]
    fn html_attributes_are_refused_only_where_the_rewrite_moves_a_quote() {
        for source in [
            "pie
  \"a<b\" : 1
  \"c=\" : 2
  \"d>\" : 3",
            "gitGraph
  commit id: \"a<b=\" tag: \"c>\"",
            "stateDiagram-v2
  A : 1<b
  B : k=\"v\"
  A --> B",
        ] {
            assert!(html_attribute_problem(source, false).is_some(), "{source}");
        }
        for source in [
            "stateDiagram-v2
  A : <b class=\"x\">太字</b>",
            "classDiagram
  A \"1\" --> \"*\" B : <i title=\"t\">x</i>",
            "timeline
  2024 : <span style=\"color:red\">赤</span>",
            "pie
  \"a<b\" : 1
  \"c\" : 2",
            "flowchart TD
  A[\"x = y\"] --> B",
        ] {
            assert_eq!(html_attribute_problem(source, false), None, "{source}");
        }
    }

    /// Flowcharts, sequence diagrams and ER diagrams take their labels from
    /// what `merman` read after it turned `="x"` into `='x'`, so a label
    /// written the way the rewrite exists for opens and is drawn the same.
    #[test]
    fn a_quoted_label_holding_an_attribute_opens_where_the_label_is_read_by_merman() {
        let without_render_ids = |svg: &str| {
            let mut out = String::with_capacity(svg.len());
            let mut rest = svg;
            while let Some(position) = rest.find("merman-") {
                let (head, tail) = rest.split_at(position + "merman-".len());
                out.push_str(head);
                rest = tail.trim_start_matches(|character: char| character.is_ascii_digit());
            }
            out.push_str(rest);
            out
        };
        for source in [
            "flowchart TD\n    A[\"<span style=\"color:red\">赤</span>\"] --> B",
            "flowchart TD\n    A -->|\"<b class=\"x\">y</b>\"| B",
            "flowchart TD\n    subgraph S[\"<b class=\"x\">t</b>\"]\n        A\n    end",
            "sequenceDiagram\n    A->>B: \"<b class=\"x\">太字</b>\"",
            "sequenceDiagram\n    participant A as \"<b class=\"x\">y</b>\"\n    A->>A: hi",
            "erDiagram\n    A ||--o{ B : \"<b class=\"x\">has</b>\"",
            "erDiagram\n    A {\n        string name \"<b class=\"x\">t</b>\"\n    }",
        ] {
            let document =
                parse_document(source).unwrap_or_else(|error| panic!("{source}\n{error}"));
            assert_eq!(document.structure_error(), None, "{source}");
            let emitted = document.emit();
            assert!(!emitted.contains("=\""), "{emitted}");
            let theme = crate::MermaidTheme::default();
            let drawn = crate::render_to_svg(source, &theme).expect("the source should render");
            let redrawn =
                crate::render_to_svg(&emitted, &theme).expect("the emitted text should render");
            assert_eq!(
                without_render_ids(&redrawn),
                without_render_ids(&drawn),
                "{source}\n->\n{emitted}"
            );
        }

        for source in [
            // Across lines the rewrite moves quotes from one label to another.
            "erDiagram\n    A ||--o{ B : \"x<y\"\n    B ||--o{ C : \"k=\"\n    C ||--o{ D : \"z>\"",
            // Carried as written, so it would be written back with its `"`
            // and could never be confirmed: it does not open instead.
            "flowchart TD\n    A --> B\n    click A \"https://example.com\" \"<b class=\"x\">t</b>\"",
        ] {
            let error = parse_document(source)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(error.contains("'"), "{source}: {error:?}");
        }
    }
}

/// The checks every kind that reads `%%{ … }%%` with [`preprocessing_problem`]
/// is held to, written once so that each kind's tests only say what one of its
/// statements looks like.
#[cfg(test)]
pub(crate) mod preprocessing_cases {
    use super::parse_document;
    use crate::{MermaidTheme, render_to_svg};

    /// `header` is the diagram keyword line, followed by whatever lines the
    /// kind needs before a statement; `statement` is one statement the model
    /// reads; `inside` is that statement with a directive in its middle.
    pub(crate) fn assert_directive_rules(header: &str, statement: &str, inside: &str) {
        let (keyword_line, setup) = header.split_once('\n').unwrap_or((header, ""));
        let setup = if setup.is_empty() {
            String::new()
        } else {
            format!("\n{setup}")
        };
        let refused = [
            format!("{header}\n    %%{{init: {{}}}}%% {statement}"),
            format!("{header}\n    %%{{init:\n    {{}}}}%% {statement}"),
            format!("{header}\n    %%{{init: {{}}}}%% %%{{wrap}}%% {statement}"),
            format!("{header}\n    {inside}"),
            format!("{keyword_line} %%{{init: {{}}}}%%{setup}\n    {statement}"),
            format!("%%{{init: {{}}}}%%\n{header}\n    %% メモ %%{{\n    {statement}"),
            format!("{header}\n    %% メモ %%{{init:\n    {{}}}}%% {statement}"),
            // `merman` 0.7 cut nothing without a closed directive, and the
            // `%%` made the line a comment; 0.8 cuts from the `%%{` to the end
            // of the text, as Mermaid does, so the statement below is lost.
            format!("{header}\n    %% 閉じない %%{{\n    {statement}"),
        ];
        for source in refused {
            let error = match parse_document(&source) {
                Ok(document) => panic!("{source}\nshould be refused, opened as {document:?}"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("%%{"), "{source}\nwas refused for {error}");
        }

        let opened = [
            format!("%%{{init: {{}}}}%%\n{header}\n    {statement}"),
            format!("{header}\n    %%{{init: {{}}}}%% %% メモ\n    {statement}"),
            format!(
                "{header}\n    %%{{init: {{}}}}%%\n    %% メモ %%{{init: {{}}}}%%\n    {statement}"
            ),
            // A `%%{` that never closes cuts only what follows it, which is
            // nothing but comments here.
            format!("{header}\n    {statement}\n    %% 閉じない %%{{\n    %% メモ"),
        ];
        for source in opened {
            let document = parse_document(&source)
                .unwrap_or_else(|error| panic!("{source}\nshould open: {error}"));
            assert_eq!(document.structure_error(), None, "{source}");
            let emitted = document.emit();
            render_to_svg(&emitted, &MermaidTheme::default())
                .unwrap_or_else(|error| panic!("{emitted}\nshould render: {error}"));
            let again = parse_document(&emitted)
                .unwrap_or_else(|error| panic!("{emitted}\nshould reopen: {error}"));
            assert_eq!(again.emit(), emitted, "{source}");
        }
    }
}
