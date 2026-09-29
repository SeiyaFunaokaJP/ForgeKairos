//! The Sankey diagram half of the Mermaid editor's form.
//!
//! A Sankey diagram is a list of flows between named nodes, and the nodes are
//! declared by nothing but the flows that name them. The form therefore has a
//! list of the node names, where renaming one renames it in every flow, and a
//! list of flows whose ends are picked from those names rather than typed, so
//! that a misspelt end cannot quietly add a node.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::sankey_model::{
    SankeyDocument, SankeyFlow, is_valid_sankey_node, is_valid_sankey_value,
};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button, field_frame, neighbour,
    row_controls, section_title,
};

// Mermaid reads no name outside ASCII in a Sankey diagram.
const NEW_NODE_NAME: &str = "NewNode";
const NEW_FLOW_VALUE: &str = "10";

pub(super) struct SankeyForm {
    pub(super) document: SankeyDocument,
    pub(super) nodes: Vec<NodeRow>,
    pub(super) flows: Vec<FlowRow>,
}

pub(super) struct NodeRow {
    /// The name the document's flows use for the node. A row added in the form
    /// holds a name no flow uses yet, and is not written back until one does.
    pub(super) name: String,
    pub(super) field: Entity<Editor>,
}

pub(super) struct FlowRow {
    pub(super) value: Entity<Editor>,
}

impl SankeyForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        self.nodes
            .iter()
            .map(|row| row.field.clone())
            .chain(self.flows.iter().map(|row| row.value.clone()))
            .collect()
    }

    /// Each node row's rename that `follow` accepts, as `(name the flows use,
    /// name typed)`.
    fn renames(&self, cx: &App, follow: impl Fn(&str, &str) -> bool) -> Vec<(String, String)> {
        self.nodes
            .iter()
            .filter_map(|row| {
                let typed = row.field.read(cx).text(cx).trim().to_string();
                (typed != row.name && follow(&row.name, &typed)).then(|| (row.name.clone(), typed))
            })
            .collect()
    }

    /// The Sankey diagram the form currently describes, with every flow
    /// pointed at the node names as they are now typed.
    pub(super) fn current_document(&self, cx: &App) -> SankeyDocument {
        self.document_with_renames(cx, |_, _| true)
    }

    fn document_with_renames(
        &self,
        cx: &App,
        follow: impl Fn(&str, &str) -> bool,
    ) -> SankeyDocument {
        let mut document = self.document.clone();
        for (flow, row) in document.flows.iter_mut().zip(&self.flows) {
            flow.value = row.value.read(cx).text(cx).trim().to_string();
        }
        document.rename_nodes(&self.renames(cx, follow));
        document
    }

    /// The node names as typed, each once, in the order of the rows.
    fn typed_names(&self, cx: &App) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for row in &self.nodes {
            let name = row.field.read(cx).text(cx).trim().to_string();
            if is_valid_sankey_node(&name) && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. The rest lives in `SankeyDocument::structure_error`, which the caller
/// consults right after this.
pub(super) fn problem(document: &SankeyDocument) -> Option<SharedString> {
    // `merman` refuses a Sankey diagram without a single record.
    document
        .flows
        .is_empty()
        .then(|| i18n::t!("Add at least one flow.").into())
}

impl MermaidEditor {
    pub(super) fn new_sankey_form(
        &mut self,
        document: SankeyDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SankeyForm {
        let names = document
            .node_names()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let nodes = names
            .into_iter()
            .map(|name| NodeRow {
                field: self.new_field(&name, window, cx),
                name,
            })
            .collect();
        let values = document
            .flows
            .iter()
            .map(|flow| flow.value.clone())
            .collect::<Vec<_>>();
        let flows = values
            .iter()
            .map(|value| FlowRow {
                value: self.new_field(value, window, cx),
            })
            .collect();
        SankeyForm {
            document,
            nodes,
            flows,
        }
    }

    pub(super) fn sankey(&self) -> Option<&SankeyForm> {
        match &self.form {
            Form::Sankey(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn sankey_mut(&mut self) -> Option<&mut SankeyForm> {
        match &mut self.form {
            Form::Sankey(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, so that
    /// the rows and the document go on describing the same names.
    ///
    /// A name typed onto one another row has or is still known by, or not yet
    /// a name at all, is held back until it settles: merging the two nodes at
    /// a button press would leave no row to type the first one's name back
    /// into, so only confirming merges them.
    fn sync_sankey(&mut self, cx: &mut Context<Self>) {
        let Some((document, typed)) = self.sankey().map(|form| {
            let typed = form
                .nodes
                .iter()
                .map(|row| row.field.read(cx).text(cx).trim().to_string())
                .collect::<Vec<_>>();
            let follow = |_: &str, new: &str| {
                is_valid_sankey_node(new)
                    && typed.iter().filter(|name| *name == new).count() == 1
                    && !form.nodes.iter().any(|row| row.name == new)
            };
            let document = form.document_with_renames(cx, follow);
            let names = form
                .nodes
                .iter()
                .zip(&typed)
                .map(|(row, name)| {
                    if *name == row.name || follow(&row.name, name) {
                        name.clone()
                    } else {
                        row.name.clone()
                    }
                })
                .collect::<Vec<_>>();
            (document, names)
        }) else {
            return;
        };
        let Some(form) = self.sankey_mut() else {
            return;
        };
        form.document = document;
        let mut seen: Vec<String> = Vec::new();
        let mut index = 0;
        form.nodes.retain_mut(|row| {
            let name = typed
                .get(index)
                .cloned()
                .unwrap_or_else(|| row.name.clone());
            index += 1;
            if seen.contains(&name) {
                return false;
            }
            seen.push(name.clone());
            row.name = name;
            true
        });
    }

    fn sankey_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    /// A node name no row holds yet, starting from `base`.
    fn unused_sankey_name(&self, base: &str) -> String {
        let taken = self
            .sankey()
            .map(|form| {
                form.nodes
                    .iter()
                    .map(|row| row.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        std::iter::once(base.to_string())
            .chain((2..).map(|number| format!("{base} {number}")))
            .find(|name| !taken.contains(name))
            .unwrap_or_else(|| base.to_string())
    }

    pub(super) fn add_sankey_node(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.sync_sankey(cx);
        let name = self.unused_sankey_name(NEW_NODE_NAME);
        let field = self.new_field(&name, window, cx);
        let form = self.sankey_mut()?;
        form.nodes.push(NodeRow {
            name: name.clone(),
            field,
        });
        self.sankey_changed(cx);
        Some(name)
    }

    /// Deletes a node with every flow that names it: a flow cannot be written
    /// with one end missing.
    pub(super) fn remove_sankey_node(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_sankey(cx);
        let Some(form) = self.sankey_mut() else {
            return;
        };
        if index >= form.nodes.len() {
            return;
        }
        let name = form.nodes.remove(index).name;
        let mut position = 0;
        while position < form.document.flows.len() {
            let names_it = form
                .document
                .flows
                .get(position)
                .is_some_and(|flow| flow.source == name || flow.target == name);
            if names_it {
                form.document.flows.remove(position);
                if position < form.flows.len() {
                    form.flows.remove(position);
                }
            } else {
                position += 1;
            }
        }
        self.sankey_changed(cx);
    }

    pub(super) fn add_sankey_flow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_sankey(cx);
        let mut names = self
            .sankey()
            .map(|form| {
                form.nodes
                    .iter()
                    .map(|row| row.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        while names.len() < 2 {
            let Some(name) = self.add_sankey_node(window, cx) else {
                return;
            };
            names.push(name);
        }
        let (Some(source), Some(target)) = (names.first().cloned(), names.get(1).cloned()) else {
            return;
        };
        let value = self.new_field(NEW_FLOW_VALUE, window, cx);
        let Some(form) = self.sankey_mut() else {
            return;
        };
        form.document.flows.push(SankeyFlow {
            source,
            target,
            value: NEW_FLOW_VALUE.to_string(),
        });
        form.flows.push(FlowRow { value });
        self.sankey_changed(cx);
    }

    pub(super) fn remove_sankey_flow(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_sankey(cx);
        let Some(form) = self.sankey_mut() else {
            return;
        };
        if index >= form.flows.len() || index >= form.document.flows.len() {
            return;
        }
        form.document.flows.remove(index);
        form.flows.remove(index);
        self.sankey_changed(cx);
    }

    pub(super) fn move_sankey_flow(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        self.sync_sankey(cx);
        let Some(form) = self.sankey_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.flows.len()) else {
            return;
        };
        if other >= form.document.flows.len() || index >= form.document.flows.len() {
            return;
        }
        form.document.flows.swap(index, other);
        form.flows.swap(index, other);
        self.sankey_changed(cx);
    }

    /// Points one end of a flow at the node typed as `name`.
    pub(super) fn set_sankey_end(
        &mut self,
        index: usize,
        is_source: bool,
        name: String,
        cx: &mut Context<Self>,
    ) {
        self.sync_sankey(cx);
        let Some(flow) = self
            .sankey_mut()
            .and_then(|form| form.document.flows.get_mut(index))
        else {
            return;
        };
        if is_source {
            flow.source = name;
        } else {
            flow.target = name;
        }
        self.sankey_changed(cx);
    }

    pub(super) fn render_sankey(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.sankey() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let names = form.typed_names(cx);
        vec![
            self.render_sankey_nodes(form, &current, window, cx),
            self.render_sankey_flows(form, &current, &names, window, cx),
        ]
    }

    fn render_sankey_nodes(
        &self,
        form: &SankeyForm,
        current: &SankeyDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let used = current.node_names();
        let typed = form
            .nodes
            .iter()
            .map(|row| row.field.read(cx).text(cx).trim().to_string())
            .collect::<Vec<_>>();
        let mut section = v_flex().w_full().gap_1().child(section_title(i18n::t!(
            "Nodes (renaming a node updates all of its flows)"
        )));
        for (index, row) in form.nodes.iter().enumerate() {
            let Some(name) = typed.get(index) else {
                continue;
            };
            let duplicate = typed.iter().take(index).any(|earlier| earlier == name);
            let note = if duplicate {
                Some(i18n::t!(
                    "Nodes with the same name are merged into one when applied"
                ))
            } else if !used.contains(&name.as_str()) {
                Some(i18n::t!("Unused (not written until a flow uses it)"))
            } else {
                None
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.field,
                            LABEL_FIELD_WIDTH,
                            is_valid_sankey_node(name),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.field.clone()),
                    )
                    .when_some(note, |this, note| {
                        this.child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
                    })
                    .child(
                        IconButton::new(("mermaid-sankey-node-remove", index), IconName::Trash)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text(i18n::t!(
                                "Delete (also deletes this node's flows)"
                            )))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.remove_sankey_node(index, cx)
                            })),
                    ),
            );
        }
        section
            .child(add_button(
                "mermaid-sankey-add-node",
                i18n::t!("Add Node"),
                cx.listener(|this, _, window, cx| {
                    this.add_sankey_node(window, cx);
                }),
            ))
            .into_any_element()
    }

    fn render_sankey_flows(
        &self,
        form: &SankeyForm,
        current: &SankeyDocument,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.flows.len().saturating_sub(1);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Flows (source → target, amount)")));
        for index in 0..form.flows.len() {
            let (Some(row), Some(flow)) = (form.flows.get(index), current.flows.get(index)) else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_sankey_end_menu(
                        index,
                        true,
                        &flow.source,
                        names,
                        window,
                        cx,
                    ))
                    .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                    .child(self.render_sankey_end_menu(
                        index,
                        false,
                        &flow.target,
                        names,
                        window,
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        field_frame(
                            &row.value,
                            NUMBER_FIELD_WIDTH,
                            is_valid_sankey_value(&flow.value),
                            window,
                            cx,
                        )
                        .child(row.value.clone()),
                    )
                    .child(row_controls(
                        "mermaid-sankey-flow",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_sankey_flow(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_sankey_flow(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| this.remove_sankey_flow(index, cx)),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-sankey-add-flow",
                i18n::t!("Add Flow"),
                cx.listener(|this, _, window, cx| this.add_sankey_flow(window, cx)),
            ))
            .into_any_element()
    }

    fn render_sankey_end_menu(
        &self,
        index: usize,
        is_source: bool,
        current: &str,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.to_string();
        let choices = names.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for name in choices.clone() {
                let this = this.clone();
                let is_selected = name == selected;
                menu = menu.toggleable_entry(
                    name.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let name = name.clone();
                        this.update(cx, |this, cx| {
                            this.set_sankey_end(index, is_source, name, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            ("mermaid-sankey-end", index * 2 + usize::from(is_source)),
            SharedString::from(current.to_string()),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_sankey_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "sankey_tests.rs"]
mod sankey_tests;
