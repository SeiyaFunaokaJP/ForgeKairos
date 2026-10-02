//! The modal behind `markdown::EditMermaid`.
//!
//! Mermaid has no coordinates: the picture's geometry comes out of `merman`'s
//! layout pass, so a canvas where boxes are dragged around cannot exist. What
//! *is* editable is the diagram itself — its elements, their links, their
//! labels and their order — and that is what this modal puts on screen, beside
//! a live drawing of what the fence will look like once it is written back.
//!
//! The shell is the same for every diagram kind: the header, the preview, the
//! Tab order, the validation line and the single write back. Only the middle of
//! the form changes, and each kind's half of it lives in a module of its own
//! ([`pie`], [`state`], [`sequence`], [`er`], [`class`], [`gantt`], [`mindmap`],
//! [`timeline`], [`gitgraph`], [`journey`], [`quadrant`], [`xychart`], [`radar`],
//! [`requirement`], [`c4`], [`block`], [`sankey`], [`packet`], [`venn`],
//! [`kanban`], [`treemap`], [`architecture`], [`tree_view`], [`ishikawa`],
//! [`event_modeling`]); the flowchart's stayed here because it was here first.
//!
//! Like the table editor, the buffer stays the single source of truth: the
//! modal reads one fence, edits a copy, and writes the whole body back with one
//! `Buffer::edit` so that undo takes the diagram back in one step (ADR-F51).

mod architecture;
mod block;
mod c4;
mod class;
mod class_menu;
mod er;
mod event_modeling;
mod gantt;
mod gitgraph;
mod ishikawa;
mod journey;
mod kanban;
mod mindmap;
mod packet;
mod pie;
mod quadrant;
mod radar;
mod requirement;
mod sankey;
mod sequence;
mod state;
mod timeline;
mod tree_view;
mod treemap;
mod venn;
mod xychart;

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use editor::actions::{Backtab, Tab};
use editor::markdown_actions::MermaidFenceTarget;
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ImageSource,
    RenderImage, SMOOTH_SVG_SCALE_FACTOR, ScrollHandle, Size, Subscription, Task,
    TextStyleRefinement, WeakEntity, Window, img, px, size,
};
use mermaid_render::flowchart_model::{
    ArrowKind, CLICK_CLASS, EDGE_STYLE_PRESETS, FlowDirection, FlowchartClassDef,
    FlowchartDocument, FlowchartEdge, FlowchartNode, FlowchartSubgraph, NodeShape, StrokeKind,
    declared_class_names, is_valid_class_name, is_valid_node_id, is_valid_style_text,
    preserved_link_style_positions, shape_data_target,
};
use mermaid_render::mermaid_model::{MermaidDiagramKind, MermaidDocument, parse_document};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Modal,
    ModalFooter, ModalHeader, Section, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};
use zed_actions::preview::markdown::EditMermaid;

use crate::markdown_preview_view::MarkdownPreviewView;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &EditMermaid, window, cx| {
        let Some(editor) =
            MarkdownPreviewView::resolve_active_item_as_markdown_editor(workspace, cx)
        else {
            // Reachable from the command palette with anything at all in front,
            // where returning in silence leaves the user guessing. The preview
            // pane is an active item of its own, so the message asks for the
            // editor rather than for the file.
            workspace.show_error(
                anyhow::anyhow!(i18n::t!("Select a Markdown editor, then run this again.")),
                cx,
            );
            return;
        };
        // A Markdown document with no diagram in it still has somewhere to put
        // one, so the action starts a fence rather than refusing.
        let target = editor.update(cx, |editor, cx| {
            let target = editor
                .mermaid_fence_target(cx)
                .or_else(|| editor.new_mermaid_fence_target(cx))?;
            // The diagram may be nowhere near the cursor, and confirming the
            // modal rewrites it, so the editor is moved onto it first.
            editor.reveal_mermaid_fence_target(&target, window, cx);
            Some(target)
        });
        open(workspace, editor, target, window, cx);
    });
}

/// Opens the modal on `target`, or explains why it cannot be opened.
///
/// Shared with the preview's per-diagram edit button, which resolves its own
/// target from the diagram the user pressed rather than from the cursor.
pub fn open(
    workspace: &mut Workspace,
    editor: Entity<Editor>,
    target: Option<MermaidFenceTarget>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(target) = target else {
        workspace.show_error(
            anyhow::anyhow!(i18n::t!("No Mermaid diagram code block was found.")),
            cx,
        );
        return;
    };

    // The modal writes through the editor, which drops the edit when the buffer
    // cannot take it. Saying so now is better than letting the user rebuild the
    // diagram and watch the result go nowhere.
    if editor.read(cx).read_only(cx) {
        workspace.show_error(
            anyhow::anyhow!(i18n::t!(
                "This buffer is read-only, so the diagram cannot be edited."
            )),
            cx,
        );
        return;
    }

    // An empty fence has no kind yet; the modal opens on the picker instead.
    let document = if target.source.trim().is_empty() {
        None
    } else if mermaid_render::mermaid_model::detect_kind(&target.source).is_none() {
        workspace.show_error(anyhow::anyhow!(unsupported_kind_message()), cx);
        return;
    } else {
        match parse_document(&target.source) {
            Ok(document) => Some(document),
            // A diagram that will not open is either malformed or spelled in a
            // way the form cannot hold without dropping part of it; either way
            // the reason comes from the parser and is shown as it is, and the
            // fence is left exactly as it was.
            Err(error) => {
                workspace.show_error(
                    anyhow::anyhow!(i18n::t!(
                        "Cannot open the Mermaid diagram in the structured editor: {error}"
                    )),
                    cx,
                );
                return;
            }
        }
    };

    workspace.toggle_modal(window, cx, move |window, cx| {
        MermaidEditor::new(editor.downgrade(), target, document, window, cx)
    });
}

/// What opening a diagram the structured editor has no form for says. The
/// kinds it does have a form for are listed, so that the user can tell a kind
/// the editor never opens from one it refused over a detail.
fn unsupported_kind_message() -> String {
    let kinds = MermaidDiagramKind::ALL
        .iter()
        .map(|kind| kind.label())
        .collect::<Vec<_>>()
        .join(i18n::tc!("list separator", ", "));
    i18n::t!(
        "This diagram is not supported by the structured editor. Edit it as text. (Diagrams the structured editor supports: {kinds})"
    )
}

/// The single line editors that back one node row.
struct NodeRow {
    id: Entity<Editor>,
    label: Entity<Editor>,
}

struct SubgraphRow {
    id: Entity<Editor>,
    title: Entity<Editor>,
}

/// The fields of one edge row besides its label: the id the source gives the
/// link and what its `linkStyle` statement says.
struct EdgeFields {
    id: Entity<Editor>,
    style: Entity<Editor>,
}

/// One `classDef` statement.
struct ClassDefRow {
    name: Entity<Editor>,
    style: Entity<Editor>,
    /// The name the definition had when the diagram was opened, or `None` for
    /// one added since. Renaming a definition renames the class wherever it is
    /// assigned, so this is what those assignments are matched against.
    original_name: Option<String>,
}

/// What the diagram addressed when the editor opened it.
///
/// A statement that came through verbatim is only dropped when the element it
/// named is one this form answers for. A `style` or `click` line whose second
/// word was never an id of this diagram is left exactly as it was found rather
/// than read as a statement about a node that has gone.
struct OriginalTargets {
    ids: Vec<String>,
    edge_ids: Vec<String>,
}

/// Everything the flowchart form holds: the structure that is not typed into a
/// text field, and one field per editable string.
struct FlowchartForm {
    document: FlowchartDocument,
    nodes: Vec<NodeRow>,
    edge_labels: Vec<Entity<Editor>>,
    subgraphs: Vec<SubgraphRow>,
    /// A class name typed by hand, offered beside the `classDef` names in every
    /// row's class menu. There is no UI for writing a `classDef` line, so
    /// without it only classes the document already declares could be assigned.
    class_input: Entity<Editor>,
    original: OriginalTargets,
    /// For each edge now in the form, the position it had when the diagram was
    /// opened, or `None` for one added since. `linkStyle` addresses an edge by
    /// that position, so this is what those statements are renumbered from.
    edge_origins: Vec<Option<usize>>,
    /// How many edges the diagram had when it was opened. A `linkStyle` number
    /// beyond it is one this form cannot account for, and is left alone.
    original_edge_count: usize,
    /// Held beside `edge_labels`, index for index.
    edge_fields: Vec<EdgeFields>,
    /// For each edge position the diagram had when it was opened, whether a
    /// `linkStyle` statement kept verbatim — one naming several edges, say —
    /// already styles it. Writing another one would apply alongside it.
    fixed_link_styles: Vec<bool>,
    class_defs: Vec<ClassDefRow>,
}

impl FlowchartForm {
    fn link_style_is_fixed(&self, edge: usize) -> bool {
        self.edge_origins
            .get(edge)
            .copied()
            .flatten()
            .is_some_and(|origin| self.fixed_link_styles.get(origin) == Some(&true))
    }

    /// The name a definition row currently gives its class. `default` is not a
    /// class anything is assigned to but the style every node starts from, so
    /// that one keeps its name.
    fn class_def_name(row: &ClassDefRow, cx: &App) -> String {
        match &row.original_name {
            Some(name) if name == DEFAULT_CLASS => name.clone(),
            _ => row.name.read(cx).text(cx).trim().to_string(),
        }
    }
}

/// The `classDef` name that styles every node rather than naming a class.
const DEFAULT_CLASS: &str = "default";

/// The half of the form that depends on which diagram is being edited.
enum Form {
    /// The fence is empty and the kind has not been picked yet.
    Choosing,
    Flowchart(FlowchartForm),
    Pie(pie::PieForm),
    State(state::StateForm),
    Sequence(sequence::SequenceForm),
    Er(er::ErForm),
    Class(class::ClassForm),
    Gantt(gantt::GanttForm),
    Mindmap(mindmap::MindmapForm),
    Timeline(timeline::TimelineForm),
    GitGraph(gitgraph::GitGraphForm),
    Journey(journey::JourneyForm),
    QuadrantChart(quadrant::QuadrantForm),
    XyChart(xychart::XyChartForm),
    Radar(radar::RadarForm),
    Requirement(requirement::RequirementForm),
    C4(c4::C4Form),
    Block(block::BlockForm),
    Sankey(sankey::SankeyForm),
    Packet(packet::PacketForm),
    Venn(venn::VennForm),
    Kanban(kanban::KanbanForm),
    Treemap(treemap::TreemapForm),
    Architecture(architecture::ArchitectureForm),
    TreeView(tree_view::TreeViewForm),
    Ishikawa(ishikawa::IshikawaForm),
    EventModeling(event_modeling::EventModelingForm),
}

/// The drawing of the source the form currently describes.
#[derive(Default)]
struct Preview {
    image: Option<Arc<RenderImage>>,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
}

pub struct MermaidEditor {
    editor: WeakEntity<Editor>,
    target: MermaidFenceTarget,
    form: Form,
    preview: Preview,
    message: Option<SharedString>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    subscriptions: Vec<Subscription>,
}

impl MermaidEditor {
    fn new(
        editor: WeakEntity<Editor>,
        target: MermaidFenceTarget,
        document: Option<MermaidDocument>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            editor,
            target,
            form: Form::Choosing,
            preview: Preview::default(),
            message: None,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            subscriptions: Vec::new(),
        };
        if let Some(document) = document {
            this.set_document(document, window, cx);
        }
        this.schedule_preview(cx);
        this
    }

    /// Builds the form for `document`, replacing whatever was on screen. Only
    /// the kind picker leads here twice, and it does so before anything has
    /// been typed.
    fn set_document(
        &mut self,
        document: MermaidDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.form = Form::Choosing;
        self.subscriptions.clear();
        self.form = match document {
            MermaidDocument::Flowchart(document) => {
                Form::Flowchart(self.new_flowchart_form(document, window, cx))
            }
            MermaidDocument::Pie(document) => Form::Pie(self.new_pie_form(document, window, cx)),
            MermaidDocument::State(document) => {
                Form::State(self.new_state_form(document, window, cx))
            }
            MermaidDocument::Sequence(document) => {
                Form::Sequence(self.new_sequence_form(document, window, cx))
            }
            MermaidDocument::Er(document) => Form::Er(self.new_er_form(document, window, cx)),
            MermaidDocument::Class(document) => {
                Form::Class(self.new_class_form(document, window, cx))
            }
            MermaidDocument::Gantt(document) => {
                Form::Gantt(self.new_gantt_form(document, window, cx))
            }
            MermaidDocument::Mindmap(document) => {
                Form::Mindmap(self.new_mindmap_form(document, window, cx))
            }
            MermaidDocument::Timeline(document) => {
                Form::Timeline(self.new_timeline_form(document, window, cx))
            }
            MermaidDocument::GitGraph(document) => {
                Form::GitGraph(self.new_gitgraph_form(document, window, cx))
            }
            MermaidDocument::Journey(document) => {
                Form::Journey(self.new_journey_form(document, window, cx))
            }
            MermaidDocument::QuadrantChart(document) => {
                Form::QuadrantChart(self.new_quadrant_form(document, window, cx))
            }
            MermaidDocument::XyChart(document) => {
                Form::XyChart(self.new_xychart_form(document, window, cx))
            }
            MermaidDocument::Radar(document) => {
                Form::Radar(self.new_radar_form(document, window, cx))
            }
            MermaidDocument::Requirement(document) => {
                Form::Requirement(self.new_requirement_form(document, window, cx))
            }
            MermaidDocument::C4(document) => Form::C4(self.new_c4_form(document, window, cx)),
            MermaidDocument::Block(document) => {
                Form::Block(self.new_block_form(document, window, cx))
            }
            MermaidDocument::Sankey(document) => {
                Form::Sankey(self.new_sankey_form(document, window, cx))
            }
            MermaidDocument::Packet(document) => {
                Form::Packet(self.new_packet_form(document, window, cx))
            }
            MermaidDocument::Venn(document) => Form::Venn(self.new_venn_form(document, window, cx)),
            MermaidDocument::Kanban(document) => {
                Form::Kanban(self.new_kanban_form(document, window, cx))
            }
            MermaidDocument::Treemap(document) => {
                Form::Treemap(self.new_treemap_form(document, window, cx))
            }
            MermaidDocument::Architecture(document) => {
                Form::Architecture(self.new_architecture_form(document, window, cx))
            }
            MermaidDocument::TreeView(document) => {
                Form::TreeView(self.new_tree_view_form(document, window, cx))
            }
            MermaidDocument::Ishikawa(document) => {
                Form::Ishikawa(self.new_ishikawa_form(document, window, cx))
            }
            MermaidDocument::EventModeling(document) => {
                Form::EventModeling(self.new_event_modeling_form(document, window, cx))
            }
        };
        self.message = None;
        self.schedule_preview(cx);
        cx.notify();
    }

    fn new_flowchart_form(
        &mut self,
        document: FlowchartDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FlowchartForm {
        let mut nodes = Vec::with_capacity(document.nodes.len());
        for node in &document.nodes {
            let (id, label) = (node.id.clone(), node.label.clone());
            nodes.push(NodeRow {
                id: self.new_field(&id, window, cx),
                label: self.new_field(&label, window, cx),
            });
        }
        let mut edge_labels = Vec::with_capacity(document.edges.len());
        let mut edge_fields = Vec::with_capacity(document.edges.len());
        for edge in &document.edges {
            let label = edge.label.clone().unwrap_or_default();
            edge_labels.push(self.new_field(&label, window, cx));
            let (id, style) = (
                edge.user_id.clone().unwrap_or_default(),
                edge.link_style.clone().unwrap_or_default(),
            );
            edge_fields.push(self.new_edge_fields(&id, &style, window, cx));
        }
        let mut class_defs = Vec::with_capacity(document.class_defs.len());
        for definition in &document.class_defs {
            let (name, style) = (definition.name.clone(), definition.style.clone());
            class_defs.push(ClassDefRow {
                name: self.new_field(&name, window, cx),
                style: self.new_class_style_field(&style, window, cx),
                original_name: Some(name),
            });
        }
        let mut subgraphs = Vec::with_capacity(document.subgraphs.len());
        for group in &document.subgraphs {
            let (id, title) = (group.id.clone(), group.title.clone());
            subgraphs.push(SubgraphRow {
                id: self.new_field(&id, window, cx),
                title: self.new_field(&title, window, cx),
            });
        }
        let class_input = self.new_field("", window, cx);
        let original = OriginalTargets {
            ids: document
                .nodes
                .iter()
                .map(|node| node.id.clone())
                .chain(document.subgraphs.iter().map(|group| group.id.clone()))
                .collect(),
            edge_ids: document
                .edges
                .iter()
                .filter_map(|edge| edge.user_id.clone())
                .collect(),
        };
        let original_edge_count = document.edges.len();
        let styled_elsewhere = preserved_link_style_positions(&document);
        FlowchartForm {
            fixed_link_styles: (0..original_edge_count)
                .map(|position| styled_elsewhere.contains(&position))
                .collect(),
            document,
            nodes,
            edge_labels,
            subgraphs,
            class_input,
            original,
            edge_origins: (0..original_edge_count).map(Some).collect(),
            original_edge_count,
            edge_fields,
            class_defs,
        }
    }

    fn new_class_style_field(
        &mut self,
        style: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let field = self.new_field(style, window, cx);
        field.update(cx, |field, cx| {
            field.set_placeholder_text(i18n::t!("e.g. fill:#f96,stroke:#333"), window, cx);
        });
        field
    }

    fn new_edge_fields(
        &mut self,
        id: &str,
        style: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EdgeFields {
        let style = self.new_field(style, window, cx);
        style.update(cx, |style, cx| {
            style.set_placeholder_text(i18n::t!("e.g. stroke:#f00,stroke-width:2px"), window, cx);
        });
        EdgeFields {
            id: self.new_field(id, window, cx),
            style,
        }
    }

    fn flowchart(&self) -> Option<&FlowchartForm> {
        match &self.form {
            Form::Flowchart(form) => Some(form),
            _ => None,
        }
    }

    fn flowchart_mut(&mut self) -> Option<&mut FlowchartForm> {
        match &mut self.form {
            Form::Flowchart(form) => Some(form),
            _ => None,
        }
    }

    /// A single line editor whose edits refresh the preview.
    fn new_field(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let settings = ThemeSettings::get_global(cx);
        // The fields hold buffer text — node ids and labels that end up in the
        // document — so they follow the buffer font the user already sized to be
        // readable rather than the smaller UI text.
        let text_style = TextStyleRefinement {
            font_family: Some(settings.buffer_font.family.clone()),
            font_features: Some(settings.buffer_font.features.clone()),
            font_size: Some(settings.buffer_font_size(cx).into()),
            font_weight: Some(settings.buffer_font.weight),
            ..Default::default()
        };
        let field = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(text.to_string(), window, cx);
            editor.set_text_style_refinement(text_style);
            editor
        });
        self.subscriptions.push(
            cx.subscribe(&field, |this, _field, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.schedule_preview(cx);
                    // The invalid-value frames are drawn from the field text,
                    // so they have to be redrawn as it changes.
                    cx.notify();
                }
            }),
        );
        field
    }

    /// The ids the id fields currently hold, which is what the edge endpoint
    /// menus and the group membership boxes offer.
    fn node_ids(&self, cx: &App) -> Vec<String> {
        self.flowchart()
            .map(|form| {
                form.nodes
                    .iter()
                    .map(|row| row.id.read(cx).text(cx).trim().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The document the form currently describes, or `None` while no kind has
    /// been picked.
    fn current_document(&self, cx: &App) -> Option<MermaidDocument> {
        match &self.form {
            Form::Choosing => None,
            Form::Flowchart(form) => Some(MermaidDocument::Flowchart(Self::current_flowchart(
                form, cx,
            ))),
            Form::Pie(form) => Some(MermaidDocument::Pie(form.current_document(cx))),
            Form::State(form) => Some(MermaidDocument::State(form.current_document(cx))),
            Form::Sequence(form) => Some(MermaidDocument::Sequence(form.current_document(cx))),
            Form::Er(form) => Some(MermaidDocument::Er(form.current_document(cx))),
            Form::Class(form) => Some(MermaidDocument::Class(form.current_document(cx))),
            Form::Gantt(form) => Some(MermaidDocument::Gantt(form.current_document(cx))),
            Form::Mindmap(form) => Some(MermaidDocument::Mindmap(form.current_document(cx))),
            Form::Timeline(form) => Some(MermaidDocument::Timeline(form.current_document(cx))),
            Form::GitGraph(form) => Some(MermaidDocument::GitGraph(form.current_document(cx))),
            Form::Journey(form) => Some(MermaidDocument::Journey(form.current_document(cx))),
            Form::QuadrantChart(form) => {
                Some(MermaidDocument::QuadrantChart(form.current_document(cx)))
            }
            Form::XyChart(form) => Some(MermaidDocument::XyChart(form.current_document(cx))),
            Form::Radar(form) => Some(MermaidDocument::Radar(form.current_document(cx))),
            Form::Requirement(form) => {
                Some(MermaidDocument::Requirement(form.current_document(cx)))
            }
            Form::C4(form) => Some(MermaidDocument::C4(form.current_document(cx))),
            Form::Block(form) => Some(MermaidDocument::Block(form.current_document(cx))),
            Form::Sankey(form) => Some(MermaidDocument::Sankey(form.current_document(cx))),
            Form::Packet(form) => Some(MermaidDocument::Packet(form.current_document(cx))),
            Form::Venn(form) => Some(MermaidDocument::Venn(form.current_document(cx))),
            Form::Kanban(form) => Some(MermaidDocument::Kanban(form.current_document(cx))),
            Form::Treemap(form) => Some(MermaidDocument::Treemap(form.current_document(cx))),
            Form::Architecture(form) => {
                Some(MermaidDocument::Architecture(form.current_document(cx)))
            }
            Form::TreeView(form) => Some(MermaidDocument::TreeView(form.current_document(cx))),
            Form::Ishikawa(form) => Some(MermaidDocument::Ishikawa(form.current_document(cx))),
            Form::EventModeling(form) => {
                Some(MermaidDocument::EventModeling(form.current_document(cx)))
            }
        }
    }

    /// The flowchart the form currently describes: the structure held in
    /// `document` with the text fields' current contents written over it.
    ///
    /// Everything else in the document refers to a node or a group by id, so
    /// renaming one in its field has to carry through to the edges, the group
    /// membership and the statements that were kept verbatim. Otherwise the old
    /// id would still be written out and Mermaid would draw an extra, empty box
    /// under it.
    fn current_flowchart(form: &FlowchartForm, cx: &App) -> FlowchartDocument {
        let mut document = form.document.clone();
        let mut renames: Vec<(String, String)> = Vec::new();
        for (node, row) in document.nodes.iter_mut().zip(&form.nodes) {
            let id = row.id.read(cx).text(cx).trim().to_string();
            if id != node.id {
                renames.push((std::mem::replace(&mut node.id, id.clone()), id));
            }
            node.label = row.label.read(cx).text(cx).to_string();
        }
        for (edge, field) in document.edges.iter_mut().zip(&form.edge_labels) {
            let label = field.read(cx).text(cx);
            edge.label = (!label.is_empty()).then_some(label);
        }
        let mut edge_renames: Vec<(String, String)> = Vec::new();
        let mut kept_edge_ids: Vec<String> = Vec::new();
        for (index, (edge, fields)) in document.edges.iter_mut().zip(&form.edge_fields).enumerate()
        {
            let id = fields.id.read(cx).text(cx).trim().to_string();
            let id = (!id.is_empty()).then_some(id);
            if let (Some(old), Some(new)) = (&edge.user_id, &id) {
                kept_edge_ids.push(old.clone());
                if old != new {
                    edge_renames.push((old.clone(), new.clone()));
                }
            }
            edge.user_id = id;
            if !form.link_style_is_fixed(index) {
                let style = fields.style.read(cx).text(cx).trim().to_string();
                edge.link_style = (!style.is_empty()).then_some(style);
            }
        }
        let mut class_renames: Vec<(String, String)> = Vec::new();
        document.class_defs = form
            .class_defs
            .iter()
            .map(|row| {
                let name = FlowchartForm::class_def_name(row, cx);
                if let Some(original) = &row.original_name
                    && *original != name
                {
                    class_renames.push((original.clone(), name.clone()));
                }
                FlowchartClassDef {
                    name,
                    style: row.style.read(cx).text(cx).trim().to_string(),
                }
            })
            .collect();
        for (group, row) in document.subgraphs.iter_mut().zip(&form.subgraphs) {
            let id = row.id.read(cx).text(cx).trim().to_string();
            if id != group.id {
                renames.push((std::mem::replace(&mut group.id, id.clone()), id));
            }
            group.title = row.title.read(cx).text(cx).to_string();
        }

        let renamed = |id: &String| {
            renames
                .iter()
                .find(|(old, _)| old == id)
                .map(|(_, new)| new.clone())
        };
        for edge in &mut document.edges {
            if let Some(id) = renamed(&edge.from) {
                edge.from = id;
            }
            if let Some(id) = renamed(&edge.to) {
                edge.to = id;
            }
        }
        for group in &mut document.subgraphs {
            for node_id in &mut group.node_ids {
                if let Some(id) = renamed(node_id) {
                    *node_id = id;
                }
            }
        }
        for edge in &mut document.edges {
            if let Some(id) = edge.subgraph.as_ref().and_then(renamed) {
                edge.subgraph = Some(id);
            }
        }
        let classes = document
            .nodes
            .iter_mut()
            .map(|node| &mut node.classes)
            .chain(
                document
                    .subgraphs
                    .iter_mut()
                    .map(|group| &mut group.classes),
            );
        for classes in classes {
            rename_classes(classes, &class_renames);
        }
        renumber_link_styles(&mut document, &form.edge_origins, form.original_edge_count);
        rename_edge_metadata(
            &mut document,
            &edge_renames,
            &kept_edge_ids,
            &form.original.edge_ids,
        );
        retarget_preserved_lines(&mut document, &renames, &form.original);
        document
    }

    /// The class names the class menus offer: the ones the document declares,
    /// the ones it already assigns, and whatever is being typed into the class
    /// field.
    fn class_candidates(&self, cx: &App) -> Vec<String> {
        let Some(form) = self.flowchart() else {
            return Vec::new();
        };
        let document = Self::current_flowchart(form, cx);
        let mut names = declared_class_names(&document);
        let assigned = document
            .nodes
            .iter()
            .map(|node| &node.classes)
            .chain(document.subgraphs.iter().map(|group| &group.classes));
        for name in assigned.flatten() {
            if name != CLICK_CLASS && !names.iter().any(|known| known == name) {
                names.push(name.clone());
            }
        }
        let typed = form.class_input.read(cx).text(cx).trim().to_string();
        if is_valid_class_name(&typed) && typed != CLICK_CLASS && !names.contains(&typed) {
            names.push(typed);
        }
        names
    }

    fn toggle_class(
        &mut self,
        target: ClassTarget,
        index: usize,
        class: &str,
        cx: &mut Context<Self>,
    ) {
        // The menus offer the names the definition rows hold now, while the
        // assignments still use the names the diagram was opened with until the
        // rename is applied on the way out.
        let Some((stored, opened_names)) = self.flowchart().map(|form| {
            let stored = form
                .class_defs
                .iter()
                .find_map(|row| {
                    (FlowchartForm::class_def_name(row, cx) == class)
                        .then(|| row.original_name.clone())
                        .flatten()
                })
                .unwrap_or_else(|| class.to_string());
            let opened_names = form
                .class_defs
                .iter()
                .filter_map(|row| row.original_name.clone())
                .collect::<Vec<_>>();
            (stored, opened_names)
        }) else {
            return;
        };
        // A node may also carry the shown name itself, with no definition
        // opened under it: the renames leave that one as it is, so it shows
        // ticked under the same entry and has to come off with it.
        let shows_as_chosen =
            |name: &String| *name == stored || (name == class && !opened_names.contains(name));
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        let classes = match target {
            ClassTarget::Node => form
                .document
                .nodes
                .get_mut(index)
                .map(|node| &mut node.classes),
            ClassTarget::Subgraph => form
                .document
                .subgraphs
                .get_mut(index)
                .map(|group| &mut group.classes),
        };
        let Some(classes) = classes else {
            return;
        };
        if classes.iter().any(shows_as_chosen) {
            classes.retain(|name| !shows_as_chosen(name));
        } else {
            classes.push(stored);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    /// The first thing wrong with what the user typed, in the words shown above
    /// the form, or `None` when the diagram can be written back.
    fn problem(document: &MermaidDocument) -> Option<SharedString> {
        let field_problem = match document {
            MermaidDocument::Flowchart(document) => Self::flowchart_problem(document),
            MermaidDocument::Pie(document) => pie::problem(document),
            MermaidDocument::State(document) => state::problem(document),
            MermaidDocument::Sequence(document) => sequence::problem(document),
            MermaidDocument::Er(document) => er::problem(document),
            MermaidDocument::Class(document) => class::problem(document),
            MermaidDocument::Gantt(document) => gantt::problem(document),
            MermaidDocument::Mindmap(document) => mindmap::problem(document),
            MermaidDocument::Timeline(document) => timeline::problem(document),
            MermaidDocument::GitGraph(document) => gitgraph::problem(document),
            MermaidDocument::Journey(document) => journey::problem(document),
            MermaidDocument::QuadrantChart(document) => quadrant::problem(document),
            MermaidDocument::XyChart(document) => xychart::problem(document),
            MermaidDocument::Radar(document) => radar::problem(document),
            MermaidDocument::Requirement(document) => requirement::problem(document),
            MermaidDocument::C4(document) => c4::problem(document),
            MermaidDocument::Block(document) => block::problem(document),
            MermaidDocument::Sankey(document) => sankey::problem(document),
            MermaidDocument::Packet(document) => packet::problem(document),
            MermaidDocument::Venn(document) => venn::problem(document),
            MermaidDocument::Kanban(document) => kanban::problem(document),
            MermaidDocument::Treemap(document) => treemap::problem(document),
            MermaidDocument::Architecture(document) => architecture::problem(document),
            MermaidDocument::TreeView(document) => tree_view::problem(document),
            MermaidDocument::Ishikawa(document) => ishikawa::problem(document),
            MermaidDocument::EventModeling(document) => event_modeling::problem(document),
        };
        field_problem.or_else(|| document.structure_error().map(SharedString::from))
    }

    /// The first thing wrong with the ids the user typed. Edges and groups refer
    /// to nodes by id, so an id that Mermaid cannot read breaks more than the
    /// node it is on.
    fn flowchart_problem(document: &FlowchartDocument) -> Option<SharedString> {
        let mut seen: Vec<&str> = Vec::new();
        for id in document
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .chain(document.subgraphs.iter().map(|group| group.id.as_str()))
        {
            if id.is_empty() {
                return Some(i18n::t!("An element has an empty ID.").into());
            }
            if !is_valid_node_id(id) {
                return Some(
                    i18n::t!(
                        "The ID \"{id}\" is not valid (ASCII letters, digits and _ only; no Mermaid keywords)."
                    )
                    .into(),
                );
            }
            if seen.contains(&id) {
                return Some(i18n::t!("The ID \"{id}\" is used more than once.").into());
            }
            seen.push(id);
        }
        Self::flowchart_edge_problem(document)
            .or_else(|| Self::flowchart_class_def_problem(document))
    }

    /// The first thing wrong with an edge's id or its `linkStyle`.
    fn flowchart_edge_problem(document: &FlowchartDocument) -> Option<SharedString> {
        let styled_elsewhere = preserved_link_style_positions(document);
        let mut seen: Vec<&str> = Vec::new();
        for (index, edge) in document.edges.iter().enumerate() {
            let position = index + 1;
            if let Some(id) = edge.user_id.as_deref() {
                if !is_valid_node_id(id) {
                    return Some(
                        i18n::t!(
                            "The ID \"{id}\" of link {position} is not valid (ASCII letters, digits and _ only; no Mermaid keywords)."
                        )
                        .into(),
                    );
                }
                if seen.contains(&id) {
                    return Some(i18n::t!("The link ID \"{id}\" is used more than once.").into());
                }
                // `id@{ … }` goes to the edge whenever an edge has that id, so
                // a node of the same name would lose its shape to it.
                if document.node_index(id).is_some()
                    || document.subgraphs.iter().any(|group| group.id == id)
                {
                    return Some(
                        i18n::t!("The link ID \"{id}\" is the same as the ID of a node or group.")
                            .into(),
                    );
                }
                seen.push(id);
            }
            let Some(style) = edge.link_style.as_deref() else {
                continue;
            };
            if !is_valid_style_text(style) || style.starts_with(',') {
                return Some(
                    i18n::t!(
                        "The line style of link {position} has characters that cannot be used (; or %%, an unclosed quote, a trailing {{, or a leading comma)."
                    )
                    .into(),
                );
            }
            if styled_elsewhere.contains(&index) {
                return Some(
                    i18n::t!(
                        "Link {position} is already styled by a linkStyle line that is kept as text."
                    )
                    .into(),
                );
            }
            // The statement is written before the kept `e1@{ … }` lines, which
            // can set the curve as well and would have the last word.
            let has_metadata = edge.user_id.as_deref().is_some_and(|id| {
                document
                    .preserved_lines
                    .iter()
                    .any(|line| shape_data_target(&line.text) == Some(id))
            });
            if has_metadata && style.contains("interpolate") {
                return Some(
                    i18n::t!(
                        "Link {position} has a {id}@{{ … }} line, so its line style cannot set interpolate (the curve).",
                        id = edge.user_id.as_deref().unwrap_or_default()
                    )
                    .into(),
                );
            }
        }
        None
    }

    /// The first thing wrong with a `classDef` row.
    fn flowchart_class_def_problem(document: &FlowchartDocument) -> Option<SharedString> {
        let mut seen: Vec<&str> = Vec::new();
        for definition in &document.class_defs {
            let name = definition.name.as_str();
            if name.is_empty() {
                return Some(i18n::t!("A class definition has an empty name.").into());
            }
            if !is_valid_class_name(name) {
                return Some(
                    i18n::t!(
                        "The class definition name \"{name}\" is not valid (no whitespace or symbols such as , ; : # %)."
                    )
                    .into(),
                );
            }
            if seen.contains(&name) {
                return Some(
                    i18n::t!("The class definition \"{name}\" is defined more than once.").into(),
                );
            }
            seen.push(name);
            if !is_valid_style_text(&definition.style) {
                return Some(
                    i18n::t!(
                        "The style of the class definition \"{name}\" has characters that cannot be used (; or %%, an unclosed quote, or a trailing {{)."
                    )
                    .into(),
                );
            }
        }
        None
    }

    /// Redraws the preview a beat after the last change, so that typing a label
    /// does not run the layout engine once per keystroke.
    fn schedule_preview(&mut self, cx: &mut Context<Self>) {
        let theme = markdown::build_mermaid_theme(cx);
        let svg_renderer = cx.svg_renderer();
        self.preview._task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let Some(described) = this
                .update(cx, |this, cx| {
                    this.current_document(cx)
                        .map(|document| (document.emit(), Self::problem(&document)))
                })
                .ok()
            else {
                return;
            };
            let Some((source, problem)) = described else {
                // Nothing has been picked yet, so there is nothing to draw.
                this.update(cx, |this, cx| {
                    this.preview.image = None;
                    this.preview.error = None;
                    cx.notify();
                })
                .ok();
                return;
            };
            if problem.is_some() {
                // The warning above the form is where a bad value is reported,
                // and the last good drawing stays on screen beside it.
                this.update(cx, |this, cx| {
                    this.message = problem;
                    cx.notify();
                })
                .ok();
                return;
            }

            let rendered = cx
                .background_spawn({
                    async move {
                        let svg = mermaid_render::render_to_svg(&source, &theme)?;
                        let tree = svg_renderer
                            .parse_svg(svg.as_bytes())
                            .map_err(|error| anyhow::anyhow!("{error}"))?;
                        svg_renderer
                            .render_parsed(&tree, 1.0)
                            .map_err(|error| anyhow::anyhow!("{error}"))
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                this.message = None;
                match rendered {
                    Ok(image) => {
                        this.preview.image = Some(image);
                        this.preview.error = None;
                    }
                    Err(error) => this.preview.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn add_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart() else {
            return;
        };
        let id = Self::current_flowchart(form, cx).unused_id("node");
        let row = NodeRow {
            id: self.new_field(&id, window, cx),
            label: self.new_field(&id, window, cx),
        };
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        form.document.nodes.push(FlowchartNode {
            label: id.clone(),
            id,
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        form.nodes.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn remove_node(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        if index >= form.nodes.len() {
            return;
        }
        let removed = form.document.nodes.remove(index).id;
        form.nodes.remove(index);
        // An edge or a group that still pointed at the node would declare it
        // again as a bare id, quietly bringing it back as an empty box. The
        // edges go from the back so that the label fields, which are held
        // alongside the edges by index, stay lined up with them.
        let orphaned = form
            .document
            .edges
            .iter()
            .enumerate()
            .filter(|(_, edge)| edge.from == removed || edge.to == removed)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in orphaned.into_iter().rev() {
            form.document.edges.remove(index);
            if index < form.edge_labels.len() {
                form.edge_labels.remove(index);
            }
            if index < form.edge_origins.len() {
                form.edge_origins.remove(index);
            }
            if index < form.edge_fields.len() {
                form.edge_fields.remove(index);
            }
        }
        for group in &mut form.document.subgraphs {
            group.node_ids.retain(|node_id| *node_id != removed);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn move_node(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.nodes.len()) else {
            return;
        };
        form.document.nodes.swap(index, other);
        form.nodes.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn add_edge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart() else {
            return;
        };
        let Some(first) = form.document.nodes.first().map(|node| node.id.clone()) else {
            self.message = Some(i18n::t!("Add a node first.").into());
            cx.notify();
            return;
        };
        let second = form
            .document
            .nodes
            .get(1)
            .map(|node| node.id.clone())
            .unwrap_or_else(|| first.clone());
        let field = self.new_field("", window, cx);
        let fields = self.new_edge_fields("", "", window, cx);
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        form.document.edges.push(FlowchartEdge {
            from: first,
            to: second,
            user_id: None,
            label: None,
            markdown_label: false,
            arrow: ArrowKind::Arrow,
            stroke: StrokeKind::Normal,
            length: 1,
            link_style: None,
            subgraph: None,
        });
        form.edge_labels.push(field);
        form.edge_origins.push(None);
        form.edge_fields.push(fields);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn remove_edge(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        if index >= form.document.edges.len() {
            return;
        }
        form.document.edges.remove(index);
        if index < form.edge_labels.len() {
            form.edge_labels.remove(index);
        }
        if index < form.edge_origins.len() {
            form.edge_origins.remove(index);
        }
        if index < form.edge_fields.len() {
            form.edge_fields.remove(index);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn move_edge(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.document.edges.len()) else {
            return;
        };
        form.document.edges.swap(index, other);
        if other < form.edge_labels.len() && index < form.edge_labels.len() {
            form.edge_labels.swap(index, other);
        }
        if other < form.edge_origins.len() && index < form.edge_origins.len() {
            form.edge_origins.swap(index, other);
        }
        if other < form.edge_fields.len() && index < form.edge_fields.len() {
            form.edge_fields.swap(index, other);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn add_subgraph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart() else {
            return;
        };
        let id = Self::current_flowchart(form, cx).unused_id("group");
        let row = SubgraphRow {
            id: self.new_field(&id, window, cx),
            title: self.new_field(&id, window, cx),
        };
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        form.document.subgraphs.push(FlowchartSubgraph {
            title: id.clone(),
            id,
            direction: None,
            node_ids: Vec::new(),
            classes: Vec::new(),
            parent: None,
        });
        form.subgraphs.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn remove_subgraph(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        if index >= form.subgraphs.len() {
            return;
        }
        let removed = form.document.subgraphs.remove(index).id;
        form.subgraphs.remove(index);
        // A nested group is a member of its parent under its own id, and left
        // there it would come back as a node of that name.
        for group in &mut form.document.subgraphs {
            group.node_ids.retain(|node_id| *node_id != removed);
        }
        for edge in &mut form.document.edges {
            if edge.subgraph.as_ref() == Some(&removed) {
                edge.subgraph = None;
            }
        }
        // Nesting is held as an index into the list that just shifted.
        for group in &mut form.document.subgraphs {
            group.parent = match group.parent {
                Some(parent) if parent == index => None,
                Some(parent) if parent > index => Some(parent - 1),
                parent => parent,
            };
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    /// `None` leaves the group without a `direction` line, so that it follows
    /// the diagram's own direction.
    fn set_subgraph_direction(
        &mut self,
        index: usize,
        direction: Option<FlowDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        let Some(group) = form.document.subgraphs.get_mut(index) else {
            return;
        };
        group.direction = direction;
        self.schedule_preview(cx);
        cx.notify();
    }

    fn add_class_def(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart() else {
            return;
        };
        // A name some node carries, or that another row had when it was
        // opened, is skipped as well: assignments hold the names rows were
        // opened with until the renames are applied on the way out, and the
        // new row's must only ever be the ones the user made from it.
        let current = Self::current_flowchart(form, cx);
        let mut taken = declared_class_names(&current);
        for document in [&current, &form.document] {
            taken.extend(
                document
                    .nodes
                    .iter()
                    .map(|node| &node.classes)
                    .chain(document.subgraphs.iter().map(|group| &group.classes))
                    .flatten()
                    .cloned(),
            );
        }
        taken.extend(
            form.class_defs
                .iter()
                .filter_map(|row| row.original_name.clone()),
        );
        let mut name = "class".to_string();
        let mut suffix = 1;
        while taken.contains(&name) {
            name = format!("class{suffix}");
            suffix += 1;
        }
        let row = ClassDefRow {
            name: self.new_field(&name, window, cx),
            style: self.new_class_style_field("", window, cx),
            original_name: Some(name),
        };
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        form.class_defs.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Drops the definition only. The nodes that carry the class keep it: a
    /// class without a definition is still valid Mermaid, and an `init`
    /// directive's `themeCSS` may be what styles it.
    fn remove_class_def(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        if index >= form.class_defs.len() {
            return;
        }
        form.class_defs.remove(index);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn move_class_def(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.class_defs.len()) else {
            return;
        };
        form.class_defs.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn toggle_membership(&mut self, group_index: usize, node_id: &str, cx: &mut Context<Self>) {
        let Some(form) = self.flowchart_mut() else {
            return;
        };
        let Some(group) = form.document.subgraphs.get_mut(group_index) else {
            return;
        };
        if let Some(position) = group.node_ids.iter().position(|id| id == node_id) {
            group.node_ids.remove(position);
        } else {
            group.node_ids.push(node_id.to_string());
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::SecondaryConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(document) = self.current_document(cx) else {
            self.message = Some(i18n::t!("Choose a diagram type.").into());
            cx.notify();
            return;
        };
        if let Some(problem) = Self::problem(&document) {
            self.message = Some(problem);
            cx.notify();
            return;
        }

        let source = document.emit();
        // The last line of defence: a document that emits source the parser
        // cannot read back would leave the fence in a state the editor could
        // not reopen, so it is not written at all.
        if let Err(error) = parse_document(&source) {
            self.message = Some(i18n::t!("This content cannot be written back: {error}").into());
            cx.notify();
            return;
        }

        let target = &self.target;
        let written = self
            .editor
            .update(cx, |editor, cx| {
                editor.write_mermaid_fence(target, &source, window, cx)
            })
            .log_err();
        match written {
            // The fence changed under the modal — another pane, a reload — so
            // the edit is dropped and the modal stays open with the work in it.
            Some(Err(error)) => {
                self.message = Some(format!("{error}").into());
                cx.notify();
            }
            _ => cx.emit(DismissEvent),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Every text field, in the order they are drawn, which is the order Tab
    /// walks them in.
    fn fields(&self) -> Vec<Entity<Editor>> {
        match &self.form {
            Form::Choosing => Vec::new(),
            Form::Flowchart(form) => {
                let nodes = form
                    .nodes
                    .iter()
                    .flat_map(|row| [row.id.clone(), row.label.clone()]);
                let edges = form
                    .edge_labels
                    .iter()
                    .enumerate()
                    .flat_map(|(index, label)| {
                        let mut fields = vec![label.clone()];
                        if let Some(extra) = form.edge_fields.get(index) {
                            fields.push(extra.id.clone());
                            if !form.link_style_is_fixed(index) {
                                fields.push(extra.style.clone());
                            }
                        }
                        fields
                    });
                let subgraphs = form
                    .subgraphs
                    .iter()
                    .flat_map(|row| [row.id.clone(), row.title.clone()]);
                let class_defs = form.class_defs.iter().flat_map(|row| {
                    let fixed_name = row.original_name.as_deref() == Some(DEFAULT_CLASS);
                    (!fixed_name)
                        .then(|| row.name.clone())
                        .into_iter()
                        .chain([row.style.clone()])
                });
                nodes
                    .chain([form.class_input.clone()])
                    .chain(edges)
                    .chain(subgraphs)
                    .chain(class_defs)
                    .collect()
            }
            Form::Pie(form) => form.fields(),
            Form::State(form) => form.fields(),
            Form::Sequence(form) => form.fields(),
            Form::Er(form) => form.fields(),
            Form::Class(form) => form.fields(),
            Form::Gantt(form) => form.fields(),
            Form::Mindmap(form) => form.fields(),
            Form::Timeline(form) => form.fields(),
            Form::GitGraph(form) => form.fields(),
            Form::Journey(form) => form.fields(),
            Form::QuadrantChart(form) => form.fields(),
            Form::XyChart(form) => form.fields(),
            Form::Radar(form) => form.fields(),
            Form::Requirement(form) => form.fields(),
            Form::C4(form) => form.fields(),
            Form::Block(form) => form.fields(),
            Form::Sankey(form) => form.fields(),
            Form::Packet(form) => form.fields(),
            Form::Venn(form) => form.fields(),
            Form::Kanban(form) => form.fields(),
            Form::Treemap(form) => form.fields(),
            Form::Architecture(form) => form.fields(),
            Form::TreeView(form) => form.fields(),
            Form::Ishikawa(form) => form.fields(),
            Form::EventModeling(form) => form.fields(),
        }
    }

    fn step_field(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields();
        if fields.is_empty() {
            return;
        }
        let focused = fields
            .iter()
            .position(|field| field.focus_handle(cx).contains_focused(window, cx));
        let next = match focused {
            Some(index) if forward => (index + 1) % fields.len(),
            Some(index) => (index + fields.len() - 1) % fields.len(),
            None => 0,
        };
        if let Some(field) = fields.get(next) {
            field.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn focus_next_field(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_field(true, window, cx);
        cx.stop_propagation();
    }

    fn focus_previous_field(&mut self, _: &Backtab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_field(false, window, cx);
        cx.stop_propagation();
    }

    /// The buttons an empty fence opens on, one per diagram kind the structured
    /// editor can hold.
    fn render_kind_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut buttons = h_flex().w_full().flex_wrap().gap_1();
        for kind in MermaidDiagramKind::ALL {
            buttons = buttons.child(
                Button::new(
                    SharedString::from(format!("mermaid-kind-{kind:?}")),
                    kind.label(),
                )
                .style(ButtonStyle::Outlined)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_document(MermaidDocument::empty(kind), window, cx);
                })),
            );
        }
        v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Diagram Type")))
            .child(buttons)
            // The form is built from the kind that was picked and there is no
            // way back to this screen, so the way out is said here.
            .child(section_title(i18n::t!(
                "To choose again, cancel and open the editor again."
            )))
            .into_any_element()
    }

    fn render_direction(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.flowchart() else {
            return div().into_any_element();
        };
        let current = form.document.direction;
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for direction in FlowDirection::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    direction.label(),
                    direction == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            if let Some(form) = this.flowchart_mut() {
                                form.document.direction = direction;
                            }
                            this.schedule_preview(cx);
                            cx.notify();
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        h_flex()
            .gap_2()
            .child(
                Label::new(i18n::t!("Direction"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                DropdownMenu::new("mermaid-direction", current.label(), menu)
                    .style(DropdownStyle::Outlined)
                    .trigger_size(ButtonSize::Compact),
            )
            .into_any_element()
    }

    fn render_nodes(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.flowchart() else {
            return div().into_any_element();
        };
        let last = form.nodes.len().saturating_sub(1);
        let candidates = self.class_candidates(cx);
        let current = Self::current_flowchart(form, cx);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Nodes")));
        for index in 0..form.nodes.len() {
            let Some(row) = form.nodes.get(index) else {
                continue;
            };
            let Some(node) = current.nodes.get(index) else {
                continue;
            };
            let id_is_valid = is_valid_node_id(row.id.read(cx).text(cx).trim());
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(&row.id, ID_FIELD_WIDTH, id_is_valid, window, cx)
                            .child(row.id.clone()),
                    )
                    .child(
                        field_frame(&row.label, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(row.label.clone()),
                    )
                    .child(self.render_shape_menu(index, node.shape.clone(), window, cx))
                    .child(self.render_class_menu(
                        ClassTarget::Node,
                        index,
                        &node.classes,
                        &candidates,
                        window,
                        cx,
                    ))
                    .child(row_controls(
                        "mermaid-node",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| this.move_node(index, false, cx)),
                        cx.listener(move |this, _, _window, cx| this.move_node(index, true, cx)),
                        cx.listener(move |this, _, _window, cx| this.remove_node(index, cx)),
                    )),
            );
        }
        let typed = form.class_input.read(cx).text(cx);
        let typed_is_valid = typed.trim().is_empty() || is_valid_class_name(typed.trim());
        section
            .child(
                Button::new("mermaid-add-node", i18n::t!("Add Node"))
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.add_node(window, cx))),
            )
            // Mermaid declares a class with a `classDef` line, which the form
            // keeps verbatim but cannot write; this is where a name that has no
            // `classDef` yet is typed so that the class menus can offer it.
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(section_title(i18n::t!("Class Name")))
                    .child(
                        field_frame(
                            &form.class_input,
                            ID_FIELD_WIDTH,
                            typed_is_valid,
                            window,
                            cx,
                        )
                        .child(form.class_input.clone()),
                    )
                    .child(section_title(i18n::t!(
                        "The classDef names and the name typed here are offered under \"Class\"."
                    ))),
            )
            .into_any_element()
    }

    /// The classes assigned to one node or group, as a menu of the names on
    /// offer. Mermaid writes them as a `class <id> <names>` statement, which the
    /// model carries, so ticking one here is all there is to it.
    fn render_class_menu(
        &self,
        target: ClassTarget,
        index: usize,
        classes: &[String],
        candidates: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let assigned = classes.to_vec();
        let candidates = candidates.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            if candidates.is_empty() {
                return menu.label(i18n::t!(
                    "Write a classDef, or type a name in \"Class Name\"."
                ));
            }
            for name in candidates.clone() {
                let this = this.clone();
                let is_selected = assigned.contains(&name);
                let chosen = name.clone();
                menu = menu.toggleable_entry(
                    name,
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let chosen = chosen.clone();
                        this.update(cx, |this, cx| {
                            this.toggle_class(target, index, &chosen, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            (target.element_id(), index),
            class_menu_label(classes),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_shape_menu(
        &self,
        index: usize,
        shape: NodeShape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let current = shape.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            // A shape the menu does not offer — one of Mermaid 11's named
            // shapes — is listed as an extra entry so that picking another one
            // is not the only way to leave it.
            let mut offered = NodeShape::PRESETS.to_vec();
            if !offered.contains(&current) {
                offered.push(current.clone());
            }
            for shape in offered {
                let this = this.clone();
                let is_selected = shape == current;
                let chosen = shape.clone();
                menu = menu.toggleable_entry(
                    shape.label(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let chosen = chosen.clone();
                        this.update(cx, |this, cx| {
                            if let Some(form) = this.flowchart_mut()
                                && let Some(node) = form.document.nodes.get_mut(index)
                            {
                                node.shape = chosen;
                            }
                            this.schedule_preview(cx);
                            cx.notify();
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(("mermaid-node-shape", index), shape.label(), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    fn render_edges(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.flowchart() else {
            return div().into_any_element();
        };
        let last = form.document.edges.len().saturating_sub(1);
        let node_ids = self.node_ids(cx);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Links")));
        for index in 0..form.document.edges.len() {
            let Some(edge) = form.document.edges.get(index) else {
                continue;
            };
            let from = edge.from.clone();
            let to = edge.to.clone();
            let operator = edge_operator_label(edge);
            let mut row = h_flex()
                .w_full()
                .gap_1()
                .child(self.render_endpoint_menu(index, true, from, &node_ids, window, cx))
                .child(self.render_arrow_menu(index, operator, window, cx))
                .child(self.render_endpoint_menu(index, false, to, &node_ids, window, cx));
            if let Some(field) = form.edge_labels.get(index) {
                row = row.child(
                    field_frame(field, LABEL_FIELD_WIDTH, true, window, cx)
                        .flex_1()
                        .child(field.clone()),
                );
            }
            let row = row.child(row_controls(
                "mermaid-edge",
                index,
                index == 0,
                index >= last,
                cx.listener(move |this, _, _window, cx| this.move_edge(index, false, cx)),
                cx.listener(move |this, _, _window, cx| this.move_edge(index, true, cx)),
                cx.listener(move |this, _, _window, cx| this.remove_edge(index, cx)),
            ));
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(row)
                    .children(self.render_edge_details(index, window, cx)),
            );
        }
        section
            .child(
                Button::new("mermaid-add-edge", i18n::t!("Add Link"))
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.add_edge(window, cx))),
            )
            .into_any_element()
    }

    /// The line under an edge row: the id the source gives the link, which an
    /// `e1@{ … }` statement addresses, and the style of its `linkStyle`
    /// statement.
    fn render_edge_details(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.flowchart()?;
        let fields = form.edge_fields.get(index)?;
        let id = fields.id.read(cx).text(cx);
        let id_is_valid = id.trim().is_empty() || is_valid_node_id(id.trim());
        let style = if form.link_style_is_fixed(index) {
            // A statement naming several edges, or several naming this one,
            // stays as the user wrote it, and a second one would apply beside
            // it.
            Label::new(i18n::t!("Set by a linkStyle line in the text"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        } else {
            let text = fields.style.read(cx).text(cx);
            let text = text.trim();
            let style_is_valid =
                text.is_empty() || (is_valid_style_text(text) && !text.starts_with(','));
            field_frame(&fields.style, LABEL_FIELD_WIDTH, style_is_valid, window, cx)
                .flex_1()
                .child(fields.style.clone())
                .into_any_element()
        };
        Some(
            h_flex()
                .w_full()
                .gap_1()
                .pl_4()
                .child(section_title("ID"))
                .child(
                    field_frame(&fields.id, ID_FIELD_WIDTH, id_is_valid, window, cx)
                        .child(fields.id.clone()),
                )
                .child(section_title(i18n::t!("Line Style")))
                .child(style)
                .into_any_element(),
        )
    }

    /// The `classDef` statements, one row each. When the source has one the
    /// form cannot hold, all of them stay as written and the section says so.
    fn render_class_defs(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.flowchart() else {
            return div().into_any_element();
        };
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Class Definitions (classDef)")));
        let locked = form.class_defs.is_empty()
            && form
                .document
                .preserved_lines
                .iter()
                .any(|line| line.text.trim_start().starts_with("classDef"));
        if locked {
            return section
                .child(section_title(i18n::t!(
                    "Some classDef lines cannot be edited here, such as a line with several names or a name defined more than once (they are kept as text)."
                )))
                .into_any_element();
        }
        let last = form.class_defs.len().saturating_sub(1);
        for (index, row) in form.class_defs.iter().enumerate() {
            let name = FlowchartForm::class_def_name(row, cx);
            let name_is_fixed = row.original_name.as_deref() == Some(DEFAULT_CLASS);
            let name_element = if name_is_fixed {
                div()
                    .w(ID_FIELD_WIDTH)
                    .flex_none()
                    .child(
                        Label::new(DEFAULT_CLASS)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                let name_is_valid = is_valid_class_name(&name);
                field_frame(&row.name, ID_FIELD_WIDTH, name_is_valid, window, cx)
                    .child(row.name.clone())
                    .into_any_element()
            };
            let style = row.style.read(cx).text(cx);
            let style_is_valid = is_valid_style_text(style.trim());
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(name_element)
                    .child(
                        field_frame(&row.style, LABEL_FIELD_WIDTH, style_is_valid, window, cx)
                            .flex_1()
                            .child(row.style.clone()),
                    )
                    .child(row_controls_labelled(
                        "mermaid-class-def",
                        index,
                        index == 0,
                        index >= last,
                        i18n::t!("Delete Definition (classes already applied stay)"),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_class_def(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_class_def(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| this.remove_class_def(index, cx)),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-add-class-def",
                i18n::t!("Add Class Definition"),
                cx.listener(|this, _, window, cx| this.add_class_def(window, cx)),
            ))
            .into_any_element()
    }

    fn render_endpoint_menu(
        &self,
        index: usize,
        is_from: bool,
        current: String,
        node_ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let ids = node_ids.to_vec();
        let selected = current.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in ids.clone() {
                let this = this.clone();
                let is_selected = id == selected;
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let id = id.clone();
                        this.update(cx, |this, cx| {
                            if let Some(form) = this.flowchart_mut()
                                && let Some(edge) = form.document.edges.get_mut(index)
                            {
                                if is_from {
                                    edge.from = id;
                                } else {
                                    edge.to = id;
                                }
                            }
                            this.schedule_preview(cx);
                            cx.notify();
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            ("mermaid-edge-endpoint", index * 2 + usize::from(is_from)),
            current,
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_arrow_menu(
        &self,
        index: usize,
        current: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for preset in &EDGE_STYLE_PRESETS {
                let this = this.clone();
                let is_selected = preset.operator == selected.as_ref();
                let (arrow, stroke) = (preset.arrow, preset.stroke);
                menu = menu.toggleable_entry(
                    format!("{}  {}", preset.operator, i18n::tr(preset.label)),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            if let Some(form) = this.flowchart_mut()
                                && let Some(edge) = form.document.edges.get_mut(index)
                            {
                                edge.arrow = arrow;
                                edge.stroke = stroke;
                            }
                            this.schedule_preview(cx);
                            cx.notify();
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(("mermaid-edge-arrow", index), current, menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    fn render_subgraphs(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.flowchart() else {
            return div().into_any_element();
        };
        let node_ids = self.node_ids(cx);
        let candidates = self.class_candidates(cx);
        let current = Self::current_flowchart(form, cx);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Groups")));
        for index in 0..form.subgraphs.len() {
            let Some(row) = form.subgraphs.get(index) else {
                continue;
            };
            let Some(group) = form.document.subgraphs.get(index) else {
                continue;
            };
            let classes = current
                .subgraphs
                .get(index)
                .map(|group| group.classes.clone())
                .unwrap_or_default();
            let mut members = h_flex().w_full().flex_wrap().gap_x_2();
            for node_id in &node_ids {
                let checked = group.node_ids.contains(node_id);
                let node_id = node_id.clone();
                members = members.child(
                    Checkbox::new(
                        SharedString::from(format!("mermaid-group-{index}-{node_id}")),
                        checked.into(),
                    )
                    .label(node_id.clone())
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(
                        move |this, _: &ToggleState, _window, cx| {
                            this.toggle_membership(index, &node_id, cx);
                        },
                    )),
                );
            }
            let id_is_valid = is_valid_node_id(row.id.read(cx).text(cx).trim());
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                field_frame(&row.id, ID_FIELD_WIDTH, id_is_valid, window, cx)
                                    .child(row.id.clone()),
                            )
                            .child(
                                field_frame(&row.title, LABEL_FIELD_WIDTH, true, window, cx)
                                    .flex_1()
                                    .child(row.title.clone()),
                            )
                            .child(self.render_subgraph_direction_menu(
                                index,
                                group.direction,
                                window,
                                cx,
                            ))
                            .child(self.render_class_menu(
                                ClassTarget::Subgraph,
                                index,
                                &classes,
                                &candidates,
                                window,
                                cx,
                            ))
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("mermaid-group-remove-{index}")),
                                    IconName::Trash,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text(i18n::t!("Delete Group")))
                                .on_click(cx.listener(
                                    move |this, _, _window, cx| this.remove_subgraph(index, cx),
                                )),
                            ),
                    )
                    .child(members),
            );
        }
        section
            .child(
                Button::new("mermaid-add-subgraph", i18n::t!("Add Group"))
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.add_subgraph(window, cx))),
            )
            .into_any_element()
    }

    /// A group can lay its own nodes out in another direction than the diagram
    /// around it, which Mermaid spells as a `direction` line inside the
    /// `subgraph` block. "指定なし" drops that line, leaving the group to follow
    /// the diagram.
    fn render_subgraph_direction_menu(
        &self,
        index: usize,
        current: Option<FlowDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for choice in [None].into_iter().chain(FlowDirection::ALL.map(Some)) {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    subgraph_direction_label(choice),
                    choice == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_subgraph_direction(index, choice, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            ("mermaid-group-direction", index),
            subgraph_direction_label(current),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    /// The middle of the form: whichever sections the diagram being edited has.
    fn render_form(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match &self.form {
            Form::Choosing => vec![self.render_kind_picker(cx)],
            Form::Flowchart(_) => vec![
                self.render_nodes(window, cx),
                self.render_edges(window, cx),
                self.render_subgraphs(window, cx),
                self.render_class_defs(window, cx),
            ],
            Form::Pie(_) => self.render_pie(window, cx),
            Form::State(_) => self.render_state(window, cx),
            Form::Sequence(_) => self.render_sequence(window, cx),
            Form::Er(_) => self.render_er(window, cx),
            Form::Class(_) => self.render_class(window, cx),
            Form::Gantt(_) => self.render_gantt(window, cx),
            Form::Mindmap(_) => self.render_mindmap(window, cx),
            Form::Timeline(_) => self.render_timeline(window, cx),
            Form::GitGraph(_) => self.render_gitgraph(window, cx),
            Form::Journey(_) => self.render_journey(window, cx),
            Form::QuadrantChart(_) => self.render_quadrant(window, cx),
            Form::XyChart(_) => self.render_xychart(window, cx),
            Form::Radar(_) => self.render_radar(window, cx),
            Form::Requirement(_) => self.render_requirement(window, cx),
            Form::C4(_) => self.render_c4(window, cx),
            Form::Block(_) => self.render_block(window, cx),
            Form::Sankey(_) => self.render_sankey(window, cx),
            Form::Packet(_) => self.render_packet(window, cx),
            Form::Venn(_) => self.render_venn(window, cx),
            Form::Kanban(_) => self.render_kanban(window, cx),
            Form::Treemap(_) => self.render_treemap(window, cx),
            Form::Architecture(_) => self.render_architecture(window, cx),
            Form::TreeView(_) => self.render_tree_view(window, cx),
            Form::Ishikawa(_) => self.render_ishikawa(window, cx),
            Form::EventModeling(_) => self.render_event_modeling(window, cx),
        }
    }

    /// The row above the form: the diagram's own direction, where it has one.
    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.form {
            Form::Flowchart(_) => self.render_direction(window, cx),
            Form::State(_) => self.render_state_toolbar(window, cx),
            Form::Er(_) => self.render_er_toolbar(window, cx),
            Form::Class(_) => self.render_class_toolbar(window, cx),
            Form::GitGraph(_) => self.render_gitgraph_toolbar(window, cx),
            Form::Journey(_) => self.render_journey_toolbar(window, cx),
            Form::QuadrantChart(_) => self.render_quadrant_toolbar(window, cx),
            Form::XyChart(_) => self.render_xychart_toolbar(window, cx),
            Form::Radar(_) => self.render_radar_toolbar(window, cx),
            Form::Requirement(_) => self.render_requirement_toolbar(window, cx),
            Form::C4(_) => self.render_c4_toolbar(window, cx),
            Form::Block(_) => self.render_block_toolbar(window, cx),
            Form::Sankey(_) => self.render_sankey_toolbar(window, cx),
            Form::Packet(_) => self.render_packet_toolbar(window, cx),
            Form::Venn(_) => self.render_venn_toolbar(window, cx),
            Form::Kanban(_) => self.render_kanban_toolbar(window, cx),
            Form::Treemap(_) => self.render_treemap_toolbar(window, cx),
            Form::Architecture(_) => self.render_architecture_toolbar(window, cx),
            Form::TreeView(_) => self.render_tree_view_toolbar(window, cx),
            Form::Ishikawa(_) => self.render_ishikawa_toolbar(window, cx),
            Form::EventModeling(_) => self.render_event_modeling_toolbar(window, cx),
            Form::Choosing | Form::Pie(_) | Form::Sequence(_) | Form::Gantt(_) => {
                div().into_any_element()
            }
            Form::Mindmap(_) | Form::Timeline(_) => div().into_any_element(),
        }
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut pane = v_flex()
            .id("mermaid-preview")
            .w(PREVIEW_WIDTH)
            .flex_none()
            .gap_1()
            .child(section_title(i18n::t!("Preview")));

        if let Some(error) = &self.preview.error {
            pane = pane.child(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            );
        }
        if let Some(image) = &self.preview.image {
            let display = preview_size(image);
            pane = pane.child(
                div()
                    .id("mermaid-preview-image")
                    .w_full()
                    .max_h(PREVIEW_HEIGHT)
                    .overflow_scroll()
                    .bg(cx.theme().colors().editor_background)
                    .rounded_md()
                    .p_1()
                    .child(
                        img(ImageSource::Render(image.clone()))
                            .w(display.width)
                            .h(display.height),
                    ),
            );
        } else if self.preview.error.is_none() {
            pane = pane.child(
                Label::new(match self.form {
                    Form::Choosing => i18n::t!("Shown once a diagram type is chosen"),
                    _ => i18n::t!("Rendering…"),
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        }

        pane.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> ModalFooter {
        ModalFooter::new()
            .start_slot::<Label>(
                Label::new(i18n::t!("Tab moves between fields. Ctrl+Enter applies."))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .end_slot::<AnyElement>(
                h_flex()
                    .gap_1()
                    .child(Button::new("mermaid-cancel", i18n::t!("Cancel")).on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("mermaid-confirm", i18n::tc!("mermaid editor", "Apply"))
                            .style(ButtonStyle::Filled)
                            .disabled(matches!(self.form, Form::Choosing))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::SecondaryConfirm, window, cx)
                            })),
                    )
                    .into_any_element(),
            )
    }
}

/// The width the id fields get: enough for a short identifier without letting
/// the form's two text columns fight over the row.
const ID_FIELD_WIDTH: Rems = Rems(8.);
const LABEL_FIELD_WIDTH: Rems = Rems(12.);
const NUMBER_FIELD_WIDTH: Rems = Rems(5.);
const PREVIEW_WIDTH: Rems = Rems(28.);
const PREVIEW_HEIGHT: Rems = Rems(30.);
const MODAL_WIDTH: Rems = Rems(72.);
const FORM_HEIGHT: Rems = Rems(30.);

fn section_title(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

/// The frame around one text field. `is_valid` is what the model layer says
/// about the text the field holds; an invalid value gets a red border so that
/// the reason above the form can be traced to the row it came from.
fn field_frame(
    field: &Entity<Editor>,
    width: Rems,
    is_valid: bool,
    window: &Window,
    cx: &mut App,
) -> gpui::Div {
    let focused = field.focus_handle(cx).contains_focused(window, cx);
    let frame = ui::input_frame_sized(ui::InputFrameSize::Compact, focused, cx)
        .w(width)
        .flex_none();
    if is_valid {
        frame
    } else {
        frame.border_color(cx.theme().status().error_border)
    }
}

/// The up, down and delete buttons every list row carries.
fn row_controls(
    prefix: &str,
    index: usize,
    at_start: bool,
    at_end: bool,
    move_up: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    move_down: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    remove: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    row_controls_labelled(
        prefix,
        index,
        at_start,
        at_end,
        i18n::t!("Delete"),
        move_up,
        move_down,
        remove,
    )
}

/// The same, for a row whose deletion takes more with it than the row itself
/// and has to say so before it is pressed.
#[allow(clippy::too_many_arguments)]
fn row_controls_labelled(
    prefix: &str,
    index: usize,
    at_start: bool,
    at_end: bool,
    remove_tooltip: &'static str,
    move_up: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    move_down: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    remove: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .flex_none()
        .gap_px()
        .child(
            IconButton::new(
                SharedString::from(format!("{prefix}-up-{index}")),
                IconName::ArrowUp,
            )
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .disabled(at_start)
            .tooltip(Tooltip::text(i18n::t!("Move Up")))
            .on_click(move_up),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("{prefix}-down-{index}")),
                IconName::ArrowDown,
            )
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .disabled(at_end)
            .tooltip(Tooltip::text(i18n::t!("Move Down")))
            .on_click(move_down),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("{prefix}-remove-{index}")),
                IconName::Trash,
            )
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .tooltip(Tooltip::text(remove_tooltip))
            .on_click(remove),
        )
}

/// The "add a row" button every list section ends with.
fn add_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(id, label)
        .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
        .label_size(LabelSize::Small)
        .on_click(on_click)
}

/// Which list a class menu is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassTarget {
    Node,
    Subgraph,
}

impl ClassTarget {
    fn element_id(self) -> &'static str {
        match self {
            Self::Node => "mermaid-node-class",
            Self::Subgraph => "mermaid-group-class",
        }
    }
}

/// What a class menu's trigger says. A long list would push the row's fields
/// off the form, so it is cut short rather than shown in full.
fn class_menu_label(classes: &[String]) -> SharedString {
    if classes.is_empty() {
        return i18n::t!("Class").into();
    }
    let joined = classes.join(",");
    let mut cut = joined.chars().take(CLASS_LABEL_CHARS).collect::<String>();
    if cut.chars().count() < joined.chars().count() {
        cut.push('…');
    }
    cut.into()
}

/// How many characters of a row's class list its menu trigger shows.
const CLASS_LABEL_CHARS: usize = 12;

/// Points the `linkStyle` statements back at the edges they meant.
///
/// `linkStyle` addresses an edge by its position in the diagram, and nothing
/// else in the source repeats that number, so adding, removing or moving an
/// edge silently hands the styling to a different link. `origins` says where
/// each edge now in the form came from, which is the mapping those numbers need.
///
/// A number this form cannot account for — one that was never a position, or a
/// list that is not numbers at all, such as `linkStyle default` — leaves the
/// statement untouched: rewriting what it does not understand would be worse
/// than leaving it as the user wrote it.
fn renumber_link_styles(
    document: &mut FlowchartDocument,
    origins: &[Option<usize>],
    original_count: usize,
) {
    document.preserved_lines.retain_mut(|line| {
        let Some(span) = number_list_span(&line.text) else {
            return true;
        };
        let Some(list) = line.text.get(span.clone()).map(str::to_string) else {
            return true;
        };
        let mut kept: Vec<String> = Vec::new();
        for entry in list.split(',') {
            let Ok(number) = entry.trim().parse::<usize>() else {
                return true;
            };
            if number >= original_count {
                return true;
            }
            if let Some(position) = origins.iter().position(|origin| *origin == Some(number)) {
                kept.push(position.to_string());
            }
        }
        let rewritten = kept.join(",");
        if rewritten == list {
            return true;
        }
        // Every edge the statement styled has been deleted, so there is nothing
        // left for it to be about.
        if kept.is_empty() {
            return false;
        }
        line.text.replace_range(span, &rewritten);
        true
    });
}

/// Where in `line` a `linkStyle` statement writes the edges it addresses.
fn number_list_span(line: &str) -> Option<Range<usize>> {
    let indent = line.len() - line.trim_start().len();
    let trimmed = line.get(indent..)?;
    let rest = trimmed.strip_prefix("linkStyle")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let leading = rest.len() - rest.trim_start().len();
    let start = indent + "linkStyle".len() + leading;
    let length = comma_list_length(rest.trim_start());
    (length > 0 && line.is_char_boundary(start) && line.is_char_boundary(start + length))
        .then_some(start..start + length)
}

/// How much of `rest` is the comma separated list it opens with: an entry, then
/// a comma and the entry after it for as long as one follows. The whitespace
/// after the last entry is left out, so the span covers the list and nothing
/// else.
fn comma_list_length(rest: &str) -> usize {
    let mut end = 0;
    let mut cursor = 0;
    while let Some(remainder) = rest.get(cursor..) {
        let entry_length = remainder
            .find(|character: char| character.is_whitespace() || character == ',')
            .unwrap_or(remainder.len());
        if entry_length == 0 {
            break;
        }
        cursor += entry_length;
        end = cursor;
        let after_entry = rest.get(cursor..).unwrap_or_default();
        let spaces = after_entry.len() - after_entry.trim_start().len();
        let Some(after_comma) = after_entry.trim_start().strip_prefix(',') else {
            break;
        };
        cursor += spaces + 1 + after_comma.len() - after_comma.trim_start().len();
    }
    end
}

/// Keeps the statements that came through verbatim pointing at the elements
/// they address, and drops the ones whose element is gone.
///
/// `style A …` declares `A` as a side effect, so a statement left behind after
/// its node was deleted would bring the node back as an empty box on the next
/// parse. A renamed element keeps its statement, rewritten onto the new id.
///
/// A statement whose second word never named anything in this diagram — a
/// `style` line the user wrote for a node they have not added yet, say — is
/// neither rewritten nor dropped. Reading it as a reference to a node that has
/// gone would delete a line the editor does not understand, which is the one
/// outcome the user cannot undo from inside the form.
fn retarget_preserved_lines(
    document: &mut FlowchartDocument,
    renames: &[(String, String)],
    original: &OriginalTargets,
) {
    const TARGETED: [&str; 2] = ["style", "click"];
    let known = document
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .chain(document.subgraphs.iter().map(|group| group.id.clone()))
        .collect::<Vec<_>>();
    let live_edge_ids = document
        .edges
        .iter()
        .filter_map(|edge| edge.user_id.clone())
        .collect::<Vec<_>>();

    document.preserved_lines.retain_mut(|line| {
        // `e1@{ … }` addresses an edge by the id the source gave it. There is no
        // UI for renaming one, so the only question is whether it is still here.
        if let Some(target) = shape_data_target(&line.text) {
            return live_edge_ids.iter().any(|id| id == target)
                || !original.edge_ids.iter().any(|id| id == target);
        }
        let Some(span) = targeted_span(&line.text, &TARGETED) else {
            return true;
        };
        let Some(target) = line.text.get(span.clone()).map(str::to_string) else {
            return true;
        };
        let renamed = renames
            .iter()
            .find(|(old, _)| *old == target)
            .map(|(_, new)| new.clone());
        let current = renamed.clone().unwrap_or_else(|| target.clone());
        if known.contains(&current) || live_edge_ids.contains(&current) {
            if let Some(renamed) = renamed {
                line.text.replace_range(span, &renamed);
            }
            return true;
        }
        !(original.ids.contains(&target) || original.edge_ids.contains(&target))
    });
}

/// Renames the classes in `classes` that a definition row renamed, dropping
/// the duplicate a rename onto a name the list already held would leave.
fn rename_classes(classes: &mut Vec<String>, renames: &[(String, String)]) {
    if renames.is_empty() {
        return;
    }
    let mut renamed: Vec<String> = Vec::with_capacity(classes.len());
    for class in classes.drain(..) {
        let class = renames
            .iter()
            .find(|(old, _)| *old == class)
            .map(|(_, new)| new.clone())
            .unwrap_or(class);
        if !renamed.contains(&class) {
            renamed.push(class);
        }
    }
    *classes = renamed;
}

/// Points the kept `e1@{ … }` statements at the edge ids they were renamed to.
///
/// The statements of the edges that lost their id — deleted, or with the id
/// field cleared — go first, by the ids the diagram was opened with: once
/// another edge has been renamed onto such an id, the leftover statement would
/// read as that edge's.
fn rename_edge_metadata(
    document: &mut FlowchartDocument,
    renames: &[(String, String)],
    kept_ids: &[String],
    original_ids: &[String],
) {
    document.preserved_lines.retain(|line| {
        shape_data_target(&line.text).is_none_or(|target| {
            !original_ids.iter().any(|id| id == target) || kept_ids.iter().any(|id| id == target)
        })
    });
    for line in &mut document.preserved_lines {
        let Some(target) = shape_data_target(&line.text).map(str::to_string) else {
            continue;
        };
        let Some((_, new)) = renames.iter().find(|(old, _)| *old == target) else {
            continue;
        };
        let indent = line.text.len() - line.text.trim_start().len();
        let span = indent..indent + target.len();
        if line.text.get(span.clone()) == Some(target.as_str()) {
            line.text.replace_range(span, new);
        }
    }
}

/// Where in `line` a kept statement writes the element it addresses.
///
/// The span is returned rather than the word so that a rename can be written
/// into the line without disturbing the rest of it. Mermaid separates words
/// with any whitespace, a full width space included, so the offsets are counted
/// in bytes off the line itself rather than assumed to be one byte per space,
/// and are checked against the character boundaries before they are used.
fn targeted_span(line: &str, keywords: &[&str]) -> Option<Range<usize>> {
    let indent = line.len() - line.trim_start().len();
    let trimmed = line.get(indent..)?;
    let keyword = trimmed.split_whitespace().next()?;
    if !keywords.contains(&keyword) {
        return None;
    }
    let after_keyword = trimmed.get(keyword.len()..)?;
    let target = after_keyword.split_whitespace().next()?;
    let leading = after_keyword.len() - after_keyword.trim_start().len();
    let start = indent + keyword.len() + leading;
    let end = start + target.len();
    (line.is_char_boundary(start) && line.is_char_boundary(end)).then_some(start..end)
}

/// The index `index` swaps with when it moves, or `None` at the end it is
/// already at.
fn neighbour(index: usize, forward: bool, length: usize) -> Option<usize> {
    if forward {
        (index + 1 < length).then_some(index + 1)
    } else {
        index.checked_sub(1)
    }
}

/// The operator the arrow menu shows for an edge, which is the same spelling
/// the emitter will write.
fn edge_operator_label(edge: &FlowchartEdge) -> SharedString {
    EDGE_STYLE_PRESETS
        .iter()
        .find(|preset| preset.arrow == edge.arrow && preset.stroke == edge.stroke)
        .map(|preset| SharedString::from(preset.operator))
        .unwrap_or_else(|| SharedString::from("-->"))
}

fn subgraph_direction_label(direction: Option<FlowDirection>) -> &'static str {
    match direction {
        Some(direction) => direction.label(),
        None => i18n::t!("Not Set"),
    }
}

/// Rasters come back at [`SMOOTH_SVG_SCALE_FACTOR`] device pixels per logical
/// pixel, so the natural size is the image's own size divided by it.
fn preview_size(image: &RenderImage) -> Size<gpui::Pixels> {
    let device_size = image.size(0);
    size(
        px(device_size.width.0 as f32 / SMOOTH_SVG_SCALE_FACTOR),
        px(device_size.height.0 as f32 / SMOOTH_SVG_SCALE_FACTOR),
    )
}

impl Render for MermaidEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(window, cx);
        let sections = self.render_form(window, cx);
        let preview = self.render_preview(cx);
        let footer = self.render_footer(cx);
        let message = self.message.clone();
        let headline = match &self.form {
            Form::Choosing => SharedString::from(i18n::t!("Create Mermaid Diagram")),
            _ => SharedString::from(i18n::t!("Edit Mermaid Diagram")),
        };

        div()
            .key_context("MermaidEditor")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(MODAL_WIDTH)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_action(cx.listener(Self::focus_next_field))
            .capture_action(cx.listener(Self::focus_previous_field))
            .child(
                Modal::new("mermaid-editor", None)
                    .header(ModalHeader::new().headline(headline).description(i18n::t!(
                        "Mermaid lays out diagrams automatically (positions cannot be set). Here you edit the diagram's elements, links and text."
                    )))
                    .section(
                        Section::new()
                            .child(toolbar)
                            .children(message.map(|message| {
                                Label::new(message)
                                    .size(LabelSize::Small)
                                    .color(Color::Warning)
                            }))
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_start()
                                    .gap_3()
                                    .child(
                                        v_flex()
                                            .id("mermaid-form")
                                            .flex_1()
                                            .gap_3()
                                            .max_h(FORM_HEIGHT)
                                            .overflow_scroll()
                                            .track_scroll(&self.scroll_handle)
                                            .children(sections),
                                    )
                                    .child(preview),
                            ),
                    )
                    .footer(footer),
            )
    }
}

impl Focusable for MermaidEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.fields().first() {
            Some(field) => field.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for MermaidEditor {}

impl ModalView for MermaidEditor {
    /// The modal holds anchors into the fence it read and a live handle on the
    /// editor it will write to; reviving a stashed copy later would write over
    /// a range the buffer has moved on from.
    fn stash_when_dismissed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests;
