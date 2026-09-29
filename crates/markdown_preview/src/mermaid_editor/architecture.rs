//! The architecture diagram half of the Mermaid editor's form.
//!
//! An architecture diagram is three lists — groups, the services and junctions
//! drawn in them, and the edges between them — so the form is those three
//! lists. A group or node names the group it sits in, and an edge its two
//! ends, by id, so those are picked from menus rather than typed, and renaming
//! an id carries into every `in` and every edge that named it. The operations
//! that take other rows with them live in the model
//! ([`mermaid_render::architecture_model`]), which the tests there pin down
//! without a window.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window, rems};
use mermaid_render::architecture_model::{
    ArchitectureDocument, ArchitectureEdge, ArchitectureEndpoint, ArchitectureGroup,
    ArchitectureIcon, ArchitectureNode, ArchitectureNodeKind, ArchitectureSide, KNOWN_ICONS,
    icon_field_text, icon_from_field, is_valid_architecture_diagram_title,
    is_valid_architecture_icon, is_valid_architecture_icon_name, is_valid_architecture_id,
    is_valid_architecture_label,
};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, ToggleState,
    Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour,
    row_controls, row_controls_labelled, section_title,
};

/// The icon fields hold a short name, so they get less room than a title.
const ICON_FIELD_WIDTH: gpui::Rems = rems(6.);

const NEW_GROUP_TITLE: &str = i18n::msg!("Group");
const NEW_SERVICE_TITLE: &str = i18n::msg!("Service");

pub(super) struct ArchitectureForm {
    pub(super) document: ArchitectureDocument,
    pub(super) title: Entity<Editor>,
    pub(super) groups: Vec<ItemRow>,
    pub(super) nodes: Vec<ItemRow>,
    pub(super) edges: Vec<EdgeRow>,
}

/// The fields of a group, service or junction row. A junction has no icon or
/// title, and its two fields are kept but not drawn.
pub(super) struct ItemRow {
    pub(super) id: Entity<Editor>,
    pub(super) icon: Entity<Editor>,
    pub(super) title: Entity<Editor>,
}

pub(super) struct EdgeRow {
    pub(super) title: Entity<Editor>,
}

/// Which list a row whose icon or parent is being set is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemList {
    Groups,
    Nodes,
}

impl ArchitectureForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let groups = self
            .groups
            .iter()
            .flat_map(|row| [row.id.clone(), row.icon.clone(), row.title.clone()]);
        let nodes = self
            .nodes
            .iter()
            .zip(&self.document.nodes)
            .flat_map(|(row, node)| match node.kind {
                ArchitectureNodeKind::Service => {
                    vec![row.id.clone(), row.icon.clone(), row.title.clone()]
                }
                ArchitectureNodeKind::Junction => vec![row.id.clone()],
            });
        std::iter::once(self.title.clone())
            .chain(groups)
            .chain(nodes)
            .chain(self.edges.iter().map(|row| row.title.clone()))
            .collect()
    }

    /// The architecture diagram the form currently describes: the structure
    /// held in `document` with the text fields' current contents written over
    /// it, and every reference pointed at the ids as they are now spelled.
    pub(super) fn current_document(&self, cx: &App) -> ArchitectureDocument {
        self.document_with_renames(cx, |_, _| true)
    }

    /// The document the fields describe, with the references carried along
    /// only by the renames `follow` accepts. A rename it refuses leaves the
    /// stored id as it was, so the rename is still pending once it settles.
    fn document_with_renames(
        &self,
        cx: &App,
        follow: impl Fn(&str, &str) -> bool,
    ) -> ArchitectureDocument {
        let mut document = self.document.clone();
        let title = field_text(&self.title, cx);
        document.title = (!title.is_empty()).then_some(title);

        let mut renames: Vec<(String, String)> = Vec::new();
        for (group, row) in document.groups.iter_mut().zip(&self.groups) {
            let id = field_text(&row.id, cx);
            if id != group.id && follow(&group.id, &id) {
                renames.push((std::mem::replace(&mut group.id, id.clone()), id));
            }
            group.icon = match field_text(&row.icon, cx).as_str() {
                "" => None,
                "()" => Some(String::new()),
                name => Some(name.to_string()),
            };
            group.title = optional_label(field_text(&row.title, cx), group.title.as_deref());
        }
        for (node, row) in document.nodes.iter_mut().zip(&self.nodes) {
            let id = field_text(&row.id, cx);
            if id != node.id && follow(&node.id, &id) {
                renames.push((std::mem::replace(&mut node.id, id.clone()), id));
            }
            if node.kind == ArchitectureNodeKind::Service {
                node.icon = icon_from_field(&field_text(&row.icon, cx));
                node.title = optional_label(field_text(&row.title, cx), node.title.as_deref());
            }
        }
        for (edge, row) in document.edges.iter_mut().zip(&self.edges) {
            edge.title = optional_label(field_text(&row.title, cx), edge.title.as_deref());
        }
        document.retarget(&renames);
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. The rest — a duplicate id, a group inside itself, an edge end that is
/// gone — comes from [`ArchitectureDocument::structure_error`].
pub(super) fn problem(document: &ArchitectureDocument) -> Option<SharedString> {
    if document.groups.is_empty() && document.nodes.is_empty() {
        return Some(i18n::t!("Add at least one group or service.").into());
    }
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

/// A title field's value. An empty field is no title, except where the fence
/// already wrote an empty `[]`, which is kept rather than rewritten.
fn optional_label(text: String, original: Option<&str>) -> Option<String> {
    if text.is_empty() {
        original
            .filter(|title| title.is_empty())
            .map(str::to_string)
    } else {
        Some(text)
    }
}

impl MermaidEditor {
    pub(super) fn new_architecture_form(
        &mut self,
        document: ArchitectureDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ArchitectureForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let mut groups = Vec::with_capacity(document.groups.len());
        for group in &document.groups {
            groups.push(self.new_group_row(group, window, cx));
        }
        let mut nodes = Vec::with_capacity(document.nodes.len());
        for node in &document.nodes {
            nodes.push(self.new_node_row(node, window, cx));
        }
        let mut edges = Vec::with_capacity(document.edges.len());
        for edge in &document.edges {
            let title = edge.title.clone().unwrap_or_default();
            edges.push(EdgeRow {
                title: self.new_field(&title, window, cx),
            });
        }
        ArchitectureForm {
            document,
            title,
            groups,
            nodes,
            edges,
        }
    }

    fn new_group_row(
        &mut self,
        group: &ArchitectureGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ItemRow {
        let icon = match group.icon.as_deref() {
            Some("") => "()".to_string(),
            icon => icon.unwrap_or_default().to_string(),
        };
        let (id, title) = (group.id.clone(), group.title.clone().unwrap_or_default());
        ItemRow {
            id: self.new_field(&id, window, cx),
            icon: self.new_field(&icon, window, cx),
            title: self.new_field(&title, window, cx),
        }
    }

    fn new_node_row(
        &mut self,
        node: &ArchitectureNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ItemRow {
        let (id, icon, title) = (
            node.id.clone(),
            icon_field_text(node.icon.as_ref()),
            node.title.clone().unwrap_or_default(),
        );
        ItemRow {
            id: self.new_field(&id, window, cx),
            icon: self.new_field(&icon, window, cx),
            title: self.new_field(&title, window, cx),
        }
    }

    pub(super) fn architecture(&self) -> Option<&ArchitectureForm> {
        match &self.form {
            Form::Architecture(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn architecture_mut(&mut self) -> Option<&mut ArchitectureForm> {
        match &mut self.form {
            Form::Architecture(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, so that
    /// the document, the rows and the references move in step.
    ///
    /// An id typed onto one that another row still holds or is still known by,
    /// or not yet an id at all, is not a rename the references can follow yet:
    /// taking them along now would hand them to the wrong row once the id is
    /// typed back.
    fn sync_architecture(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.architecture().map(|form| {
            let typed = form
                .groups
                .iter()
                .chain(&form.nodes)
                .map(|row| field_text(&row.id, cx))
                .collect::<Vec<_>>();
            let stored = form
                .document
                .groups
                .iter()
                .map(|group| group.id.as_str())
                .chain(form.document.nodes.iter().map(|node| node.id.as_str()))
                .collect::<Vec<_>>();
            form.document_with_renames(cx, |_, new| {
                is_valid_architecture_id(new)
                    && typed.iter().filter(|id| *id == new).count() == 1
                    && !stored.contains(&new)
            })
        }) else {
            return;
        };
        if let Some(form) = self.architecture_mut() {
            form.document = document;
        }
    }

    fn architecture_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_architecture_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture() else {
            return;
        };
        let group = ArchitectureGroup {
            id: form.document.unused_id("group"),
            icon: Some("cloud".to_string()),
            title: Some(i18n::tr(NEW_GROUP_TITLE).to_string()),
            parent: None,
            comment: None,
        };
        let row = self.new_group_row(&group, window, cx);
        let focus = row.id.clone();
        let Some(form) = self.architecture_mut() else {
            return;
        };
        form.document.groups.push(group);
        form.groups.push(row);
        focus.focus_handle(cx).focus(window, cx);
        self.architecture_changed(cx);
    }

    pub(super) fn add_architecture_node(
        &mut self,
        kind: ArchitectureNodeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture() else {
            return;
        };
        let node = ArchitectureNode {
            kind,
            id: form.document.unused_id(kind.keyword()),
            icon: (kind == ArchitectureNodeKind::Service)
                .then(|| ArchitectureIcon::Named("server".to_string())),
            title: (kind == ArchitectureNodeKind::Service)
                .then(|| i18n::tr(NEW_SERVICE_TITLE).to_string()),
            parent: None,
            comment: None,
        };
        let row = self.new_node_row(&node, window, cx);
        let focus = row.id.clone();
        let Some(form) = self.architecture_mut() else {
            return;
        };
        form.document.nodes.push(node);
        form.nodes.push(row);
        focus.focus_handle(cx).focus(window, cx);
        self.architecture_changed(cx);
    }

    /// Adds an edge between the first two services or junctions, or from the
    /// only one to itself, for the user to point where it belongs.
    pub(super) fn add_architecture_edge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture() else {
            return;
        };
        let ids = form
            .document
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let Some(from) = ids.first().cloned() else {
            return;
        };
        let to = ids.get(1).cloned().unwrap_or_else(|| from.clone());
        let title = self.new_field("", window, cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        form.document.edges.push(ArchitectureEdge {
            from: ArchitectureEndpoint {
                id: from,
                side: ArchitectureSide::Right,
                arrow: false,
                group_boundary: false,
            },
            to: ArchitectureEndpoint {
                id: to,
                side: ArchitectureSide::Left,
                arrow: true,
                group_boundary: false,
            },
            title: None,
            comment: None,
        });
        form.edges.push(EdgeRow { title });
        self.architecture_changed(cx);
    }

    pub(super) fn remove_architecture_item(
        &mut self,
        list: ItemList,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        let removed_edges = match list {
            ItemList::Groups => {
                if index >= form.groups.len() || index >= form.document.groups.len() {
                    return;
                }
                form.groups.remove(index);
                form.document.remove_group(index)
            }
            ItemList::Nodes => {
                if index >= form.nodes.len() || index >= form.document.nodes.len() {
                    return;
                }
                form.nodes.remove(index);
                form.document.remove_node(index)
            }
        };
        for edge in removed_edges.into_iter().rev() {
            if edge < form.edges.len() {
                form.edges.remove(edge);
            }
        }
        self.architecture_changed(cx);
    }

    pub(super) fn move_architecture_item(
        &mut self,
        list: ItemList,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        match list {
            ItemList::Groups => {
                let Some(other) = neighbour(index, forward, form.groups.len()) else {
                    return;
                };
                if index >= form.document.groups.len() || other >= form.document.groups.len() {
                    return;
                }
                form.document.groups.swap(index, other);
                form.groups.swap(index, other);
            }
            ItemList::Nodes => {
                let Some(other) = neighbour(index, forward, form.nodes.len()) else {
                    return;
                };
                if index >= form.document.nodes.len() || other >= form.document.nodes.len() {
                    return;
                }
                form.document.nodes.swap(index, other);
                form.nodes.swap(index, other);
            }
        }
        self.architecture_changed(cx);
    }

    pub(super) fn remove_architecture_edge(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        if index >= form.edges.len() || index >= form.document.edges.len() {
            return;
        }
        form.document.edges.remove(index);
        form.edges.remove(index);
        self.architecture_changed(cx);
    }

    pub(super) fn move_architecture_edge(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.edges.len()) else {
            return;
        };
        if index >= form.document.edges.len() || other >= form.document.edges.len() {
            return;
        }
        form.document.edges.swap(index, other);
        form.edges.swap(index, other);
        self.architecture_changed(cx);
    }

    pub(super) fn set_architecture_parent(
        &mut self,
        list: ItemList,
        index: usize,
        parent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        let slot = match list {
            ItemList::Groups => form
                .document
                .groups
                .get_mut(index)
                .map(|group| &mut group.parent),
            ItemList::Nodes => form
                .document
                .nodes
                .get_mut(index)
                .map(|node| &mut node.parent),
        };
        let Some(slot) = slot else {
            return;
        };
        *slot = parent;
        self.architecture_changed(cx);
    }

    /// Writes a known icon's name into a row's icon field, which is where the
    /// icon is read from.
    pub(super) fn set_architecture_icon(
        &mut self,
        list: ItemList,
        index: usize,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.architecture() else {
            return;
        };
        let rows = match list {
            ItemList::Groups => &form.groups,
            ItemList::Nodes => &form.nodes,
        };
        let Some(field) = rows.get(index).map(|row| row.icon.clone()) else {
            return;
        };
        field.update(cx, |field, cx| field.set_text(name, window, cx));
        self.architecture_changed(cx);
    }

    pub(super) fn update_architecture_edge(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut ArchitectureEdge),
    ) {
        self.sync_architecture(cx);
        let Some(form) = self.architecture_mut() else {
            return;
        };
        let Some(edge) = form.document.edges.get_mut(index) else {
            return;
        };
        edit(edge);
        self.architecture_changed(cx);
    }

    pub(super) fn render_architecture(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.architecture() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        vec![
            h_flex()
                .w_full()
                .gap_2()
                .child(section_title(i18n::t!("Title")))
                .child(
                    field_frame(
                        &form.title,
                        LABEL_FIELD_WIDTH,
                        current
                            .title
                            .as_ref()
                            .is_none_or(|title| is_valid_architecture_diagram_title(title)),
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(form.title.clone()),
                )
                .into_any_element(),
            self.render_architecture_items(form, &current, ItemList::Groups, window, cx),
            self.render_architecture_items(form, &current, ItemList::Nodes, window, cx),
            self.render_architecture_edges(form, &current, window, cx),
        ]
    }

    fn render_architecture_items(
        &self,
        form: &ArchitectureForm,
        current: &ArchitectureDocument,
        list: ItemList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (rows, heading, prefix) = match list {
            ItemList::Groups => (
                &form.groups,
                i18n::t!("Groups"),
                "mermaid-architecture-group",
            ),
            ItemList::Nodes => (
                &form.nodes,
                i18n::t!("Services and Junctions"),
                "mermaid-architecture-node",
            ),
        };
        let group_ids = current
            .groups
            .iter()
            .map(|group| group.id.clone())
            .collect::<Vec<_>>();
        let last = rows.len().saturating_sub(1);
        let mut section = v_flex().w_full().gap_1().child(section_title(heading));
        for (index, row) in rows.iter().enumerate() {
            let (id, parent, kind, icon_is_valid, title_is_valid) = match list {
                ItemList::Groups => {
                    let Some(group) = current.groups.get(index) else {
                        continue;
                    };
                    (
                        group.id.clone(),
                        group.parent.clone(),
                        None,
                        group
                            .icon
                            .as_ref()
                            .is_none_or(|icon| is_valid_architecture_icon_name(icon)),
                        group
                            .title
                            .as_ref()
                            .is_none_or(|title| is_valid_architecture_label(title)),
                    )
                }
                ItemList::Nodes => {
                    let Some(node) = current.nodes.get(index) else {
                        continue;
                    };
                    (
                        node.id.clone(),
                        node.parent.clone(),
                        Some(node.kind),
                        node.icon.as_ref().is_none_or(is_valid_architecture_icon),
                        node.title
                            .as_ref()
                            .is_none_or(|title| is_valid_architecture_label(title)),
                    )
                }
            };
            // A group cannot sit inside itself; the menu leaves it out.
            let parents = group_ids
                .iter()
                .filter(|candidate| list == ItemList::Nodes || **candidate != id)
                .cloned()
                .collect::<Vec<_>>();

            let mut line = h_flex().w_full().gap_1();
            if let Some(kind) = kind {
                line = line.child(
                    div()
                        .w(rems(5.5))
                        .flex_none()
                        .child(section_title(kind.label())),
                );
            }
            line = line.child(
                field_frame(
                    &row.id,
                    ID_FIELD_WIDTH,
                    is_valid_architecture_id(&id),
                    window,
                    cx,
                )
                .child(row.id.clone()),
            );
            if kind != Some(ArchitectureNodeKind::Junction) {
                line = line
                    .child(
                        field_frame(&row.icon, ICON_FIELD_WIDTH, icon_is_valid, window, cx)
                            .child(row.icon.clone()),
                    )
                    .child(self.render_architecture_icon_menu(list, index, window, cx))
                    .child(
                        field_frame(&row.title, LABEL_FIELD_WIDTH, title_is_valid, window, cx)
                            .flex_1()
                            .min_w_0()
                            .child(row.title.clone()),
                    );
            } else {
                line = line.child(div().flex_1());
            }
            let remove_tooltip = match list {
                ItemList::Groups => i18n::t!("Delete (the contents move to the enclosing group)"),
                ItemList::Nodes => i18n::t!("Delete (connected edges are deleted too)"),
            };
            line = line
                .child(self.render_architecture_parent_menu(
                    list,
                    index,
                    parent.as_deref(),
                    &parents,
                    window,
                    cx,
                ))
                .child(row_controls_labelled(
                    prefix,
                    index,
                    index == 0,
                    index >= last,
                    remove_tooltip,
                    cx.listener(move |this, _, _window, cx| {
                        this.move_architecture_item(list, index, false, cx)
                    }),
                    cx.listener(move |this, _, _window, cx| {
                        this.move_architecture_item(list, index, true, cx)
                    }),
                    cx.listener(move |this, _, _window, cx| {
                        this.remove_architecture_item(list, index, cx)
                    }),
                ));
            section = section.child(line);
        }

        let buttons = match list {
            ItemList::Groups => h_flex().gap_1().child(add_button(
                "mermaid-architecture-add-group",
                i18n::t!("Add Group"),
                cx.listener(|this, _, window, cx| this.add_architecture_group(window, cx)),
            )),
            ItemList::Nodes => h_flex()
                .gap_1()
                .child(add_button(
                    "mermaid-architecture-add-service",
                    i18n::t!("Add Service"),
                    cx.listener(|this, _, window, cx| {
                        this.add_architecture_node(ArchitectureNodeKind::Service, window, cx)
                    }),
                ))
                .child(add_button(
                    "mermaid-architecture-add-junction",
                    i18n::t!("Add Junction"),
                    cx.listener(|this, _, window, cx| {
                        this.add_architecture_node(ArchitectureNodeKind::Junction, window, cx)
                    }),
                )),
        };
        section.child(buttons).into_any_element()
    }

    fn render_architecture_icon_menu(
        &self,
        list: ItemList,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (name, label) in KNOWN_ICONS {
                let this = this.clone();
                menu = menu.entry(
                    format!("{} ({name})", i18n::tr(label)),
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_architecture_icon(list, index, name, window, cx)
                        })
                        .log_err();
                    },
                );
            }
            let this = this.clone();
            menu.entry(i18n::t!("No Icon"), None, move |window, cx| {
                this.update(cx, |this, cx| {
                    this.set_architecture_icon(list, index, "", window, cx)
                })
                .log_err();
            })
        });
        let prefix = match list {
            ItemList::Groups => "mermaid-architecture-group-icon",
            ItemList::Nodes => "mermaid-architecture-node-icon",
        };
        DropdownMenu::new(
            SharedString::from(format!("{prefix}-{index}")),
            i18n::t!("Suggestions"),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .trigger_tooltip(Tooltip::text(i18n::t!("Common Icons")))
        .into_any_element()
    }

    fn render_architecture_parent_menu(
        &self,
        list: ItemList,
        index: usize,
        current: Option<&str>,
        groups: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.map(str::to_string);
        let choices = groups.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let options = std::iter::once(None).chain(choices.iter().cloned().map(Some));
            for option in options {
                let this = this.clone();
                let label = option
                    .clone()
                    .unwrap_or_else(|| i18n::t!("No Group").to_string());
                let is_selected = option == selected;
                menu = menu.toggleable_entry(
                    label,
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let option = option.clone();
                        this.update(cx, |this, cx| {
                            this.set_architecture_parent(list, index, option, cx)
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        let prefix = match list {
            ItemList::Groups => "mermaid-architecture-group-parent",
            ItemList::Nodes => "mermaid-architecture-node-parent",
        };
        let label = match current {
            Some(parent) => format!("in {parent}"),
            None => i18n::t!("No Group").to_string(),
        };
        DropdownMenu::new(SharedString::from(format!("{prefix}-{index}")), label, menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .trigger_tooltip(Tooltip::text(i18n::t!("Parent Group")))
            .into_any_element()
    }

    fn render_architecture_edges(
        &self,
        form: &ArchitectureForm,
        current: &ArchitectureDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // `merman` cannot lay out an edge that ends on a group, so only
        // services and junctions are offered.
        let ids = current
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let last = form.edges.len().saturating_sub(1);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Edges")));
        for (index, row) in form.edges.iter().enumerate() {
            let Some(edge) = current.edges.get(index) else {
                continue;
            };
            let title_is_valid = edge
                .title
                .as_ref()
                .is_none_or(|title| is_valid_architecture_label(title));
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_px()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(self.render_architecture_endpoint(
                                index, true, &edge.from, &ids, window, cx,
                            ))
                            .child(
                                field_frame(
                                    &row.title,
                                    LABEL_FIELD_WIDTH,
                                    title_is_valid,
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .min_w_0()
                                .child(row.title.clone()),
                            )
                            .child(row_controls(
                                "mermaid-architecture-edge",
                                index,
                                index == 0,
                                index >= last,
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_architecture_edge(index, false, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_architecture_edge(index, true, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.remove_architecture_edge(index, cx)
                                }),
                            )),
                    )
                    .child(h_flex().w_full().gap_1().child(
                        self.render_architecture_endpoint(index, false, &edge.to, &ids, window, cx),
                    )),
            );
        }
        section
            .child(
                add_button(
                    "mermaid-architecture-add-edge",
                    i18n::t!("Add Edge"),
                    cx.listener(|this, _, window, cx| this.add_architecture_edge(window, cx)),
                )
                .into_any_element(),
            )
            .into_any_element()
    }

    /// One end of an edge: its node, the side of the node it is attached to,
    /// whether it ends in an arrow head and whether it leaves the node's group
    /// rather than the node.
    fn render_architecture_endpoint(
        &self,
        index: usize,
        is_from: bool,
        end: &ArchitectureEndpoint,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = index * 2 + usize::from(is_from);
        let set = move |edit: Box<dyn FnOnce(&mut ArchitectureEndpoint)>| {
            move |edge: &mut ArchitectureEdge| {
                edit(if is_from {
                    &mut edge.from
                } else {
                    &mut edge.to
                })
            }
        };

        let this = cx.entity().downgrade();
        let selected = end.id.clone();
        let choices = ids.to_vec();
        let id_menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
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
                            this.update_architecture_edge(
                                index,
                                cx,
                                set(Box::new(move |end| end.id = id)),
                            );
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        let this = cx.entity().downgrade();
        let current_side = end.side;
        let side_menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for side in ArchitectureSide::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    format!("{} ({})", side.label(), side.letter()),
                    side == current_side,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_architecture_edge(
                                index,
                                cx,
                                set(Box::new(move |end| end.side = side)),
                            );
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        let arrow = end.arrow;
        let group_boundary = end.group_boundary;
        h_flex()
            .flex_none()
            .gap_1()
            .child(
                div()
                    .w(rems(1.5))
                    .flex_none()
                    .child(section_title(if is_from {
                        i18n::tc!("mermaid edge end", "From")
                    } else {
                        i18n::tc!("mermaid edge end", "To")
                    })),
            )
            .child(
                DropdownMenu::new(
                    ("mermaid-architecture-edge-end", slot),
                    SharedString::from(end.id.clone()),
                    id_menu,
                )
                .style(DropdownStyle::Outlined)
                .trigger_size(ButtonSize::Compact),
            )
            .child(
                DropdownMenu::new(
                    ("mermaid-architecture-edge-side", slot),
                    end.side.label(),
                    side_menu,
                )
                .style(DropdownStyle::Outlined)
                .trigger_size(ButtonSize::Compact)
                .trigger_tooltip(Tooltip::text(i18n::t!("Connection Side"))),
            )
            .child(
                IconButton::new(
                    ("mermaid-architecture-edge-arrow", slot),
                    if is_from {
                        IconName::ArrowLeft
                    } else {
                        IconName::ArrowRight
                    },
                )
                .shape(IconButtonShape::Square)
                .icon_size(IconSize::XSmall)
                .toggle_state(arrow)
                .tooltip(Tooltip::text(if arrow {
                    i18n::t!("Arrow on. Click to remove the arrow.")
                } else {
                    i18n::t!("No arrow. Click to add an arrow.")
                }))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.update_architecture_edge(
                        index,
                        cx,
                        set(Box::new(|end| end.arrow = !end.arrow)),
                    );
                })),
            )
            .child(
                Checkbox::new(
                    ("mermaid-architecture-edge-group", slot),
                    ToggleState::from(group_boundary),
                )
                .label(i18n::t!("Group Edge"))
                .label_size(LabelSize::Small)
                .on_click(cx.listener(
                    move |this, state: &ToggleState, _window, cx| {
                        let selected = *state == ToggleState::Selected;
                        this.update_architecture_edge(
                            index,
                            cx,
                            set(Box::new(move |end| end.group_boundary = selected)),
                        );
                    },
                )),
            )
            .into_any_element()
    }

    pub(super) fn render_architecture_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "architecture_tests.rs"]
mod architecture_tests;
