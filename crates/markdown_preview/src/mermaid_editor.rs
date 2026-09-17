//! The modal behind `markdown::EditMermaid`.
//!
//! Mermaid has no coordinates: the picture's geometry comes out of `merman`'s
//! layout pass, so a canvas where boxes are dragged around cannot exist. What
//! *is* editable is the graph itself — its nodes, edges, labels, shapes,
//! direction and groups — and that is what this modal puts on screen, beside a
//! live drawing of what the fence will look like once it is written back.
//!
//! Like the table editor, the buffer stays the single source of truth: the
//! modal reads one fence, edits a copy, and writes the whole body back with one
//! `Buffer::edit` so that undo takes the diagram back in one step (ADR-F51).

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
    ArrowKind, EDGE_STYLE_PRESETS, FlowDirection, FlowchartDocument, FlowchartEdge, FlowchartNode,
    FlowchartSubgraph, NodeShape, StrokeKind, emit_flowchart, is_flowchart_source,
    is_valid_node_id, parse_flowchart,
};
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
            return;
        };
        let target = editor.update(cx, |editor, cx| editor.mermaid_fence_target(cx));
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
            anyhow::anyhow!("カーソルが Mermaid のコードブロックの中にありません。"),
            cx,
        );
        return;
    };

    // The modal writes through the editor, which drops the edit when the buffer
    // cannot take it. Saying so now is better than letting the user rebuild the
    // diagram and watch the result go nowhere.
    if editor.read(cx).read_only(cx) {
        workspace.show_error(
            anyhow::anyhow!("このバッファーは読み取り専用のため、図を編集できません。"),
            cx,
        );
        return;
    }

    let document = match parse_flowchart(&target.source) {
        Ok(document) => document,
        // A flowchart that will not open is either malformed or spelled in a
        // way the form cannot hold without dropping part of it; either way the
        // reason comes from the parser and is shown as it is.
        Err(error) if is_flowchart_source(&target.source) => {
            workspace.show_error(
                anyhow::anyhow!("Mermaid 図を構造化編集で開けません: {error}"),
                cx,
            );
            return;
        }
        Err(error) => {
            workspace.show_error(anyhow::anyhow!("{error}"), cx);
            return;
        }
    };

    workspace.toggle_modal(window, cx, move |window, cx| {
        MermaidEditor::new(editor.downgrade(), target, document, window, cx)
    });
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
    /// Everything about the diagram that is not typed into a text field: the
    /// direction, shapes, arrows, group membership and the statements that are
    /// carried through untouched.
    document: FlowchartDocument,
    nodes: Vec<NodeRow>,
    edge_labels: Vec<Entity<Editor>>,
    subgraphs: Vec<SubgraphRow>,
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
        document: FlowchartDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            editor,
            target,
            document,
            nodes: Vec::new(),
            edge_labels: Vec::new(),
            subgraphs: Vec::new(),
            preview: Preview::default(),
            message: None,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            subscriptions: Vec::new(),
        };

        for index in 0..this.document.nodes.len() {
            let node = &this.document.nodes[index];
            let (id, label) = (node.id.clone(), node.label.clone());
            let row = NodeRow {
                id: this.new_field(&id, window, cx),
                label: this.new_field(&label, window, cx),
            };
            this.nodes.push(row);
        }
        for index in 0..this.document.edges.len() {
            let label = this.document.edges[index].label.clone().unwrap_or_default();
            let field = this.new_field(&label, window, cx);
            this.edge_labels.push(field);
        }
        for index in 0..this.document.subgraphs.len() {
            let group = &this.document.subgraphs[index];
            let (id, title) = (group.id.clone(), group.title.clone());
            let row = SubgraphRow {
                id: this.new_field(&id, window, cx),
                title: this.new_field(&title, window, cx),
            };
            this.subgraphs.push(row);
        }

        this.schedule_preview(cx);
        this
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
                }
            }),
        );
        field
    }

    /// The ids the id fields currently hold, which is what the edge endpoint
    /// menus and the group membership boxes offer.
    fn node_ids(&self, cx: &App) -> Vec<String> {
        self.nodes
            .iter()
            .map(|row| row.id.read(cx).text(cx).trim().to_string())
            .collect()
    }

    /// The document the form currently describes: the structure held in
    /// `document` with the text fields' current contents written over it.
    ///
    /// Everything else in the document refers to a node or a group by id, so
    /// renaming one in its field has to carry through to the edges, the group
    /// membership and the statements that were kept verbatim. Otherwise the old
    /// id would still be written out and Mermaid would draw an extra, empty box
    /// under it.
    fn current_document(&self, cx: &App) -> FlowchartDocument {
        let mut document = self.document.clone();
        let mut renames: Vec<(String, String)> = Vec::new();
        for (node, row) in document.nodes.iter_mut().zip(&self.nodes) {
            let id = row.id.read(cx).text(cx).trim().to_string();
            if id != node.id {
                renames.push((std::mem::replace(&mut node.id, id.clone()), id));
            }
            node.label = row.label.read(cx).text(cx).to_string();
        }
        for (edge, field) in document.edges.iter_mut().zip(&self.edge_labels) {
            let label = field.read(cx).text(cx);
            edge.label = (!label.is_empty()).then_some(label);
        }
        for (group, row) in document.subgraphs.iter_mut().zip(&self.subgraphs) {
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
        retarget_preserved_lines(&mut document, &renames);
        document
    }

    /// The first thing wrong with the ids the user typed, in the words shown
    /// above the form. Edges and groups refer to nodes by id, so an id that
    /// Mermaid cannot read breaks more than the node it is on.
    fn id_problem(&self, document: &FlowchartDocument) -> Option<SharedString> {
        let mut seen: Vec<&str> = Vec::new();
        for id in document
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .chain(document.subgraphs.iter().map(|group| group.id.as_str()))
        {
            if id.is_empty() {
                return Some("ID が空の要素があります。".into());
            }
            if !is_valid_node_id(id) {
                return Some(
                    format!(
                        "ID「{id}」は使えません (半角英数字と _ のみ、Mermaid の予約語は不可)。"
                    )
                    .into(),
                );
            }
            if seen.contains(&id) {
                return Some(format!("ID「{id}」が重複しています。").into());
            }
            seen.push(id);
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
            let Some((source, problem)) = this
                .update(cx, |this, cx| {
                    let document = this.current_document(cx);
                    (emit_flowchart(&document), this.id_problem(&document))
                })
                .ok()
            else {
                return;
            };
            if problem.is_some() {
                // The warning above the form is where a bad id is reported,
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
        let id = self.current_document(cx).unused_id("node");
        let row = NodeRow {
            id: self.new_field(&id, window, cx),
            label: self.new_field(&id, window, cx),
        };
        self.document.nodes.push(FlowchartNode {
            label: id.clone(),
            id,
            markdown_label: false,
            shape: NodeShape::Rectangle,
            classes: Vec::new(),
        });
        self.nodes.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn remove_node(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.nodes.len() {
            return;
        }
        let removed = self.document.nodes.remove(index).id;
        self.nodes.remove(index);
        // An edge or a group that still pointed at the node would declare it
        // again as a bare id, quietly bringing it back as an empty box. The
        // edges go from the back so that the label fields, which are held
        // alongside the edges by index, stay lined up with them.
        let orphaned = self
            .document
            .edges
            .iter()
            .enumerate()
            .filter(|(_, edge)| edge.from == removed || edge.to == removed)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in orphaned.into_iter().rev() {
            self.document.edges.remove(index);
            if index < self.edge_labels.len() {
                self.edge_labels.remove(index);
            }
        }
        for group in &mut self.document.subgraphs {
            group.node_ids.retain(|node_id| *node_id != removed);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn move_node(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(other) = neighbour(index, forward, self.nodes.len()) else {
            return;
        };
        self.document.nodes.swap(index, other);
        self.nodes.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn add_edge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(first) = self.document.nodes.first().map(|node| node.id.clone()) else {
            self.message = Some("先にノードを追加してください。".into());
            cx.notify();
            return;
        };
        let second = self
            .document
            .nodes
            .get(1)
            .map(|node| node.id.clone())
            .unwrap_or_else(|| first.clone());
        self.document.edges.push(FlowchartEdge {
            from: first,
            to: second,
            label: None,
            markdown_label: false,
            arrow: ArrowKind::Arrow,
            stroke: StrokeKind::Normal,
            length: 1,
        });
        let field = self.new_field("", window, cx);
        self.edge_labels.push(field);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn remove_edge(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.document.edges.len() {
            return;
        }
        self.document.edges.remove(index);
        if index < self.edge_labels.len() {
            self.edge_labels.remove(index);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn move_edge(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(other) = neighbour(index, forward, self.document.edges.len()) else {
            return;
        };
        self.document.edges.swap(index, other);
        if other < self.edge_labels.len() && index < self.edge_labels.len() {
            self.edge_labels.swap(index, other);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn add_subgraph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.current_document(cx).unused_id("group");
        let row = SubgraphRow {
            id: self.new_field(&id, window, cx),
            title: self.new_field(&id, window, cx),
        };
        self.document.subgraphs.push(FlowchartSubgraph {
            title: id.clone(),
            id,
            direction: None,
            node_ids: Vec::new(),
            classes: Vec::new(),
            parent: None,
        });
        self.subgraphs.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn remove_subgraph(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.subgraphs.len() {
            return;
        }
        self.document.subgraphs.remove(index);
        self.subgraphs.remove(index);
        // Nesting is held as an index into the list that just shifted.
        for group in &mut self.document.subgraphs {
            group.parent = match group.parent {
                Some(parent) if parent == index => None,
                Some(parent) if parent > index => Some(parent - 1),
                parent => parent,
            };
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    fn toggle_membership(&mut self, group_index: usize, node_id: &str, cx: &mut Context<Self>) {
        let Some(group) = self.document.subgraphs.get_mut(group_index) else {
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
        let document = self.current_document(cx);
        if let Some(problem) = self.id_problem(&document) {
            self.message = Some(problem);
            cx.notify();
            return;
        }

        let source = emit_flowchart(&document);
        let target = &self.target;
        self.editor
            .update(cx, |editor, cx| {
                editor.write_mermaid_fence(target, &source, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Every text field, in the order they are drawn, which is the order Tab
    /// walks them in.
    fn fields(&self) -> Vec<Entity<Editor>> {
        let nodes = self
            .nodes
            .iter()
            .flat_map(|row| [row.id.clone(), row.label.clone()]);
        let subgraphs = self
            .subgraphs
            .iter()
            .flat_map(|row| [row.id.clone(), row.title.clone()]);
        nodes
            .chain(self.edge_labels.iter().cloned())
            .chain(subgraphs)
            .collect()
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

    fn render_direction(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.document.direction;
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
                            this.document.direction = direction;
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
                Label::new("方向")
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
        let last = self.nodes.len().saturating_sub(1);
        let mut section = v_flex().w_full().gap_1().child(section_title("ノード"));
        for index in 0..self.nodes.len() {
            let Some(row) = self.nodes.get(index) else {
                continue;
            };
            let Some(node) = self.document.nodes.get(index) else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(field_frame(&row.id, ID_FIELD_WIDTH, window, cx).child(row.id.clone()))
                    .child(
                        field_frame(&row.label, LABEL_FIELD_WIDTH, window, cx)
                            .flex_1()
                            .child(row.label.clone()),
                    )
                    .child(self.render_shape_menu(index, node.shape.clone(), window, cx))
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
        section
            .child(
                Button::new("mermaid-add-node", "ノードを追加")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.add_node(window, cx))),
            )
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
                            if let Some(node) = this.document.nodes.get_mut(index) {
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
        let last = self.document.edges.len().saturating_sub(1);
        let node_ids = self.node_ids(cx);
        let mut section = v_flex().w_full().gap_1().child(section_title("つながり"));
        for index in 0..self.document.edges.len() {
            let Some(edge) = self.document.edges.get(index) else {
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
            if let Some(field) = self.edge_labels.get(index) {
                row = row.child(
                    field_frame(field, LABEL_FIELD_WIDTH, window, cx)
                        .flex_1()
                        .child(field.clone()),
                );
            }
            section = section.child(row.child(row_controls(
                "mermaid-edge",
                index,
                index == 0,
                index >= last,
                cx.listener(move |this, _, _window, cx| this.move_edge(index, false, cx)),
                cx.listener(move |this, _, _window, cx| this.move_edge(index, true, cx)),
                cx.listener(move |this, _, _window, cx| this.remove_edge(index, cx)),
            )));
        }
        section
            .child(
                Button::new("mermaid-add-edge", "つながりを追加")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.add_edge(window, cx))),
            )
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
                            if let Some(edge) = this.document.edges.get_mut(index) {
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
                    format!("{}  {}", preset.operator, preset.label),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            if let Some(edge) = this.document.edges.get_mut(index) {
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
        let node_ids = self.node_ids(cx);
        let mut section = v_flex().w_full().gap_1().child(section_title("グループ"));
        for index in 0..self.subgraphs.len() {
            let Some(row) = self.subgraphs.get(index) else {
                continue;
            };
            let Some(group) = self.document.subgraphs.get(index) else {
                continue;
            };
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
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                field_frame(&row.id, ID_FIELD_WIDTH, window, cx)
                                    .child(row.id.clone()),
                            )
                            .child(
                                field_frame(&row.title, LABEL_FIELD_WIDTH, window, cx)
                                    .flex_1()
                                    .child(row.title.clone()),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("mermaid-group-remove-{index}")),
                                    IconName::Trash,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("グループを削除"))
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
                Button::new("mermaid-add-subgraph", "グループを追加")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.add_subgraph(window, cx))),
            )
            .into_any_element()
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut pane = v_flex()
            .id("mermaid-preview")
            .w(PREVIEW_WIDTH)
            .flex_none()
            .gap_1()
            .child(section_title("プレビュー"));

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
                Label::new("描画中…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }

        pane.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> ModalFooter {
        ModalFooter::new()
            .start_slot::<Label>(
                Label::new("Tab で項目を移動。Ctrl+Enter で反映")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .end_slot::<AnyElement>(
                h_flex()
                    .gap_1()
                    .child(Button::new("mermaid-cancel", "キャンセル").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("mermaid-confirm", "反映")
                            .style(ButtonStyle::Filled)
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
const PREVIEW_WIDTH: Rems = Rems(28.);
const PREVIEW_HEIGHT: Rems = Rems(30.);
const MODAL_WIDTH: Rems = Rems(72.);
const FORM_HEIGHT: Rems = Rems(30.);

fn section_title(text: &'static str) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn field_frame(field: &Entity<Editor>, width: Rems, window: &Window, cx: &mut App) -> gpui::Div {
    let focused = field.focus_handle(cx).contains_focused(window, cx);
    ui::input_frame_sized(ui::InputFrameSize::Compact, focused, cx)
        .w(width)
        .flex_none()
}

/// The up, down and delete buttons every list row carries.
fn row_controls(
    prefix: &'static str,
    index: usize,
    at_start: bool,
    at_end: bool,
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
            .tooltip(Tooltip::text("上へ移動"))
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
            .tooltip(Tooltip::text("下へ移動"))
            .on_click(move_down),
        )
        .child(
            IconButton::new(
                SharedString::from(format!("{prefix}-remove-{index}")),
                IconName::Trash,
            )
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .tooltip(Tooltip::text("削除"))
            .on_click(remove),
        )
}

/// Keeps the statements that came through verbatim pointing at the elements
/// they address, and drops the ones whose element is gone.
///
/// `style A …` declares `A` as a side effect, so a statement left behind after
/// its node was deleted would bring the node back as an empty box on the next
/// parse. A renamed element keeps its statement, rewritten onto the new id.
fn retarget_preserved_lines(document: &mut FlowchartDocument, renames: &[(String, String)]) {
    const TARGETED: [&str; 2] = ["style", "click"];
    let known = document
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .chain(document.subgraphs.iter().map(|group| group.id.clone()))
        .collect::<Vec<_>>();

    document.preserved_lines.retain_mut(|line| {
        let indent = line.text.len() - line.text.trim_start().len();
        let Some(trimmed) = line.text.get(indent..).map(str::to_string) else {
            return true;
        };
        let mut words = trimmed.split_whitespace();
        let (Some(keyword), Some(target)) = (words.next(), words.next()) else {
            return true;
        };
        if !TARGETED.contains(&keyword) {
            return true;
        }
        let renamed = renames
            .iter()
            .find(|(old, _)| old == target)
            .map(|(_, new)| new.as_str());
        if !known.iter().any(|id| id == renamed.unwrap_or(target)) {
            return false;
        }
        if let Some(renamed) = renamed {
            let Some(after_keyword) = trimmed.get(keyword.len()..) else {
                return true;
            };
            let start =
                indent + keyword.len() + after_keyword.len() - after_keyword.trim_start().len();
            line.text
                .replace_range(start..start + target.len(), renamed);
        }
        true
    });
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
        let direction = self.render_direction(window, cx);
        let nodes = self.render_nodes(window, cx);
        let edges = self.render_edges(window, cx);
        let subgraphs = self.render_subgraphs(window, cx);
        let preview = self.render_preview(cx);
        let footer = self.render_footer(cx);
        let message = self.message.clone();

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
                    .header(ModalHeader::new().headline("Mermaid 図を編集").description(
                        "Mermaid は配置を自動で決めます (座標は持てません)。ここでは要素・つながり・文字・向き・グループを編集します。",
                    ))
                    .section(
                        Section::new()
                            .child(direction)
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
                                            .child(nodes)
                                            .child(edges)
                                            .child(subgraphs),
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
        match self.nodes.first() {
            Some(row) => row.id.focus_handle(cx),
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
mod tests {
    use super::*;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::TestAppContext;
    use indoc::indoc;
    use std::sync::Arc;

    #[gpui::test]
    async fn adding_a_node_writes_the_fence_back(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ[開始] --> B[終了]
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_node(window, cx);
                let Some(row) = modal.nodes.last() else {
                    panic!("the new node should have a row");
                };
                row.id.update(cx, |id, cx| id.set_text("C", window, cx));
                row.label
                    .update(cx, |label, cx| label.set_text("追加", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let text = cx.update_editor(|editor, _window, cx| editor.text(cx));
        assert_eq!(
            text,
            indoc! {"
                ```mermaid
                flowchart TB
                    A[開始]
                    B[終了]
                    C[追加]
                    A --> B
                ```
            "},
        );
    }

    #[gpui::test]
    async fn a_duplicate_id_is_refused_and_the_fence_is_left_alone(cx: &mut TestAppContext) {
        let source = indoc! {"
            ```mermaid
            flowchart TD
              Aˇ[開始] --> B[終了]
            ```
        "};
        let (mut cx, modal, _app_state) = open_on(source, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.nodes.last() else {
                    panic!("the diagram should have two nodes");
                };
                row.id.update(cx, |id, cx| id.set_text("A", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "a diagram with two nodes called A must not be written back"
        );
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(
            message.as_deref(),
            Some("ID「A」が重複しています。"),
            "the modal should say why it refused"
        );
    }

    #[gpui::test]
    async fn an_empty_id_is_refused(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ[開始] --> B[終了]
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.nodes.first() else {
                    panic!("the diagram should have a node");
                };
                row.id.update(cx, |id, cx| id.set_text("  ", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(message.as_deref(), Some("ID が空の要素があります。"));
    }

    #[gpui::test]
    async fn a_group_collects_the_nodes_that_are_ticked(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart LR
                  Aˇ --> B
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_subgraph(window, cx);
                let Some(row) = modal.subgraphs.last() else {
                    panic!("the new group should have a row");
                };
                row.id.update(cx, |id, cx| id.set_text("g", window, cx));
                row.title
                    .update(cx, |title, cx| title.set_text("まとまり", window, cx));
                modal.toggle_membership(0, "A", cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                flowchart LR
                    A
                    B
                    A --> B
                    subgraph g [まとまり]
                        A
                    end
                ```
            "},
        );
    }

    #[gpui::test]
    async fn removing_a_node_takes_its_edges_and_memberships_with_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ -->|一| B
                  B -->|二| C
                  subgraph g [まとまり]
                    B
                    C
                  end
                ```
            "},
            cx,
        )
        .await;

        // The label fields are held beside the edges by index, so dropping the
        // middle node has to drop its two edges' fields as well.
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let removed = modal
                    .nodes
                    .iter()
                    .position(|row| row.id.read(cx).text(cx) == "B")
                    .expect("B should have a row");
                modal.remove_node(removed, cx);
                assert_eq!(modal.document.edges.len(), 0);
                assert_eq!(modal.edge_labels.len(), 0);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                flowchart TB
                    A
                    C
                    subgraph g [まとまり]
                        C
                    end
                ```
            "},
        );
    }

    /// Everything but the node's own declaration names it by id, so a rename
    /// that stopped at the declaration would leave the edges, the group and the
    /// style statement pointing at an id Mermaid would draw as an extra box.
    #[gpui::test]
    async fn renaming_a_node_follows_into_everything_that_names_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart LR
                  Aˇ[開始] --> B[終了]
                  subgraph g [まとまり]
                    A
                  end
                  style A fill:#f00
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.nodes.first() else {
                    panic!("the diagram should have a node");
                };
                row.id.update(cx, |id, cx| id.set_text("Start", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                flowchart LR
                    Start[開始]
                    B[終了]
                    Start --> B
                    subgraph g [まとまり]
                        Start
                    end
                  style Start fill:#f00
                ```
            "},
        );
    }

    /// A `style` statement declares the node it names, so one left behind would
    /// bring the deleted node back as an empty box.
    #[gpui::test]
    async fn deleting_a_node_takes_its_style_statement_with_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart LR
                  Aˇ --> B
                  style A fill:#f00
                  style B fill:#0f0
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_node(0, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                flowchart LR
                    B
                  style B fill:#0f0
                ```
            "},
        );
    }

    /// The modal holds a field editor per row, a subscription per field and a
    /// pending preview task; none of them may hold the modal itself.
    #[gpui::test]
    async fn the_modal_and_its_fields_are_released(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart TD
                  Aˇ[開始] --> B[終了]
                  subgraph g [まとまり]
                    A
                  end
                ```
            "},
            cx,
        )
        .await;

        let released = modal.downgrade();
        let fields = cx.update(|_window, cx| {
            modal
                .read(cx)
                .fields()
                .iter()
                .map(|field| field.downgrade())
                .collect::<Vec<_>>()
        });
        assert!(!fields.is_empty(), "the form should have text fields");

        // Typing leaves a preview task in flight; dropping the modal before it
        // comes back must not keep it alive.
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.nodes.first() else {
                    panic!("the diagram should have a node");
                };
                row.label
                    .update(cx, |label, cx| label.set_text("途中", window, cx));
                modal.cancel(&menu::Cancel, window, cx);
            })
        });
        drop(modal);
        cx.run_until_parked();

        released.assert_released();
        for field in fields {
            field.assert_released();
        }
    }

    #[gpui::test]
    async fn removing_one_of_two_edges_keeps_the_other_label(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                flowchart LR
                  Aˇ -->|一| B
                  B -->|二| C
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_edge(0, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                flowchart LR
                    A
                    B
                    C
                    B -->|二| C
                ```
            "},
        );
    }

    /// A fence holding a diagram the structured editor cannot express is
    /// refused before the modal is built, which is the branch `open` takes.
    #[gpui::test]
    async fn another_diagram_type_does_not_open(cx: &mut TestAppContext) {
        let _state = init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.set_state(indoc! {"
            ```mermaid
            sequenceDiagram
              Aˇ->>B: hi
            ```
        "});

        let target = cx
            .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
            .expect("the cursor is inside a mermaid fence");
        let error = parse_flowchart(&target.source)
            .err()
            .expect("a sequence diagram is not a flowchart");
        assert!(
            format!("{error}").contains("フローチャート"),
            "unexpected message: {error}"
        );
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
        cx.update(|cx| {
            let state = workspace::AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            state
        })
    }

    fn markdown_language() -> Arc<language::Language> {
        Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Markdown".into(),
                ..Default::default()
            },
            None,
        ))
    }

    /// An editor holding `state` with the modal opened on the fence the cursor
    /// marker sits in.
    async fn open_on(
        state: &str,
        cx: &mut TestAppContext,
    ) -> (
        EditorTestContext,
        Entity<MermaidEditor>,
        Arc<workspace::AppState>,
    ) {
        let app_state = init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.set_state(state);

        let editor = cx.editor.clone();
        let target = cx
            .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
            .expect("the cursor is inside a mermaid fence");
        let document = parse_flowchart(&target.source).expect("the fence holds a flowchart");
        let modal = cx.update(|window, cx| {
            cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, document, window, cx))
        });
        (cx, modal, app_state)
    }
}
