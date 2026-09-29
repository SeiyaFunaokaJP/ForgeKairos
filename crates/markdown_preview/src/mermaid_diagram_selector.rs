//! The picker behind `markdown::InsertMermaidDiagramPicker`.
//!
//! `markdown::InsertMermaidDiagram` carries the diagram type, so the command
//! palette can only ever offer its default. This modal exists so every diagram
//! type is reachable without a keybinding or a right click.

use editor::Editor;
use editor::actions::{InsertMermaidDiagram, MermaidDiagramKind};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
    Window,
};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::InsertMermaidDiagramPicker;
use crate::markdown_preview_view::MarkdownPreviewView;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &InsertMermaidDiagramPicker, window, cx| {
        let Some(editor) =
            MarkdownPreviewView::resolve_active_item_as_markdown_editor(workspace, cx)
        else {
            return;
        };
        workspace.toggle_modal(window, cx, move |window, cx| {
            MermaidDiagramSelector::new(editor.downgrade(), window, cx)
        });
    });
}

pub struct MermaidDiagramSelector {
    picker: Entity<Picker<MermaidDiagramSelectorDelegate>>,
}

impl MermaidDiagramSelector {
    fn new(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = MermaidDiagramSelectorDelegate {
            selector: cx.entity().downgrade(),
            editor,
            matches: MermaidDiagramKind::ALL.to_vec(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for MermaidDiagramSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(24.)).child(self.picker.clone())
    }
}

impl Focusable for MermaidDiagramSelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for MermaidDiagramSelector {}
impl ModalView for MermaidDiagramSelector {}

struct MermaidDiagramSelectorDelegate {
    selector: WeakEntity<MermaidDiagramSelector>,
    editor: WeakEntity<Editor>,
    matches: Vec<MermaidDiagramKind>,
    selected_index: usize,
}

/// The mermaid keyword a diagram type is written with, so typing `seq` or
/// `gantt` finds it as readily as its Japanese name does.
fn diagram_keyword(diagram: MermaidDiagramKind) -> &'static str {
    diagram.template().split_whitespace().next().unwrap_or("")
}

/// The diagram types whose Japanese name or Mermaid keyword contains `query`,
/// ignoring case, in menu order.
fn matching_diagrams(query: &str) -> Vec<MermaidDiagramKind> {
    let query = query.trim().to_lowercase();
    MermaidDiagramKind::ALL
        .into_iter()
        .filter(|diagram| {
            query.is_empty()
                || diagram.label().to_lowercase().contains(&query)
                || diagram_keyword(*diagram).to_lowercase().contains(&query)
        })
        .collect()
}

impl PickerDelegate for MermaidDiagramSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "mermaid diagram selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        i18n::t!("Select a Mermaid diagram type to insert…").into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = matching_diagrams(&query);
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(diagram) = self.matches.get(self.selected_index).copied() {
            self.editor
                .update(cx, |editor, cx| {
                    editor.insert_mermaid_diagram(&InsertMermaidDiagram { diagram }, window, cx);
                })
                .log_err();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let diagram = *self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(diagram.label()))
                .end_slot(
                    Label::new(diagram_keyword(diagram))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A template the preview does not draw would be inserted as a code block
    /// that never turns into a picture, and one `merman` cannot parse would be
    /// drawn as an error. Every entry of the insert menus has to pass both.
    #[test]
    fn every_template_is_drawn_by_the_preview() {
        for diagram in MermaidDiagramKind::ALL {
            let template = diagram.template();
            assert!(
                markdown::is_supported_diagram_type(template),
                "the preview does not draw the {} template",
                diagram.label()
            );
            let svg = mermaid_render::render_to_svg(
                template,
                &crate::markdown_export::print_mermaid_theme(),
            )
            .unwrap_or_else(|error| {
                panic!("the {} template should render: {error}", diagram.label())
            });
            assert!(
                !svg.contains("Syntax error in text"),
                "the {} template rendered an error diagram",
                diagram.label()
            );
        }
    }

    #[test]
    fn a_keyword_finds_its_diagram() {
        for (query, expected) in [
            ("xychart", MermaidDiagramKind::XyChart),
            ("XYCHART-BETA", MermaidDiagramKind::XyChart),
            ("quadrant", MermaidDiagramKind::QuadrantChart),
            ("journey", MermaidDiagramKind::Journey),
            ("user journey", MermaidDiagramKind::Journey),
            ("quadrant chart", MermaidDiagramKind::QuadrantChart),
            ("gantt", MermaidDiagramKind::Gantt),
            ("c4", MermaidDiagramKind::C4),
            ("sankey", MermaidDiagramKind::Sankey),
            ("ishikawa", MermaidDiagramKind::Ishikawa),
            ("eventmodeling", MermaidDiagramKind::EventModeling),
        ] {
            let found = matching_diagrams(query);
            assert!(found.contains(&expected), "`{query}` found {found:?}");
        }
        assert_eq!(matching_diagrams("").len(), MermaidDiagramKind::ALL.len());

        // The labels are searched in the display language, and the keywords in any.
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        for (query, expected) in [
            ("ジャーニー", MermaidDiagramKind::Journey),
            ("象限", MermaidDiagramKind::QuadrantChart),
            ("特性要因", MermaidDiagramKind::Ishikawa),
            ("journey", MermaidDiagramKind::Journey),
            ("xychart", MermaidDiagramKind::XyChart),
        ] {
            let found = matching_diagrams(query);
            assert!(found.contains(&expected), "`{query}` found {found:?}");
        }
    }
}
