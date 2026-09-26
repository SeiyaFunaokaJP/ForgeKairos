//! Double-clicking the labels of pie charts, gantt charts and ER attribute
//! cells, driven through the raster the way the user does it.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Context, Entity, IntoElement, Pixels, Render, TestAppContext, VisualTestContext, Window, point,
    px, size,
};
use mermaid_render::{DiagramGeometry, LabelItem};
use ui::prelude::*;

use super::CachedMermaidDiagram;
use crate::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
    MarkdownStyle, WrapButtonVisibility,
};

/// What the consumer receives: the range it would overwrite, the token it
/// expects there, the text the field opens on, and whether the new label
/// `"%%"` would be turned away.
type Recorded = (Option<Range<usize>>, String, String, bool);

struct TestWindow;

impl Render for TestWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct EditableMermaid {
    markdown: Entity<Markdown>,
    requests: Rc<RefCell<Vec<Recorded>>>,
}

impl Render for EditableMermaid {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let requests = self.requests.clone();
        div().child(
            MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                .code_block_renderer(CodeBlockRenderer::Default {
                    copy_button_visibility: CopyButtonVisibility::Hidden,
                    wrap_button_visibility: WrapButtonVisibility::Hidden,
                    border: false,
                })
                .on_mermaid_label_edit(move |request, _window, _cx| {
                    requests.borrow_mut().push((
                        request.source_range(),
                        request.source_text().to_string(),
                        request.current_label.to_string(),
                        request.write_refusal_message("%%").is_some(),
                    ));
                }),
        )
    }
}

fn ensure_theme_initialized(cx: &mut TestAppContext) {
    cx.update(|cx| {
        if !cx.has_global::<settings::SettingsStore>() {
            settings::init(cx);
        }
        if !cx.has_global::<theme::GlobalTheme>() {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        }
    });
}

/// Renders `source` as the only diagram of a document, with the raster and
/// geometry merman produces, and returns what a double-click on the label
/// `item` hands the consumer.
fn double_click(
    cx: &mut TestAppContext,
    source: &str,
    item: impl Fn(&LabelItem) -> bool,
) -> (Vec<Recorded>, Arc<DiagramGeometry>) {
    ensure_theme_initialized(cx);
    let (_, cx) = cx.add_window_view(|_, _| TestWindow);
    let markdown = cx.new(|cx| {
        Markdown::new_with_options(
            source.to_string().into(),
            None,
            None,
            MarkdownOptions {
                render_mermaid_diagrams: true,
                ..Default::default()
            },
            cx,
        )
    });
    cx.run_until_parked();

    let contents = markdown.read_with(cx, |markdown, _| {
        let (_, diagram) = markdown
            .parsed_markdown
            .mermaid_diagrams
            .iter()
            .next()
            .expect("the fence should have been extracted");
        diagram.contents.clone()
    });
    let (image, geometry) = markdown.update(cx, |_, cx| {
        let (svg, geometry) = mermaid_render::render_to_svg_with_geometry(
            &contents.contents,
            &super::build_mermaid_theme(cx),
        )
        .expect("the diagram should render");
        let parsed = Arc::new(
            cx.svg_renderer()
                .parse_svg(svg.as_bytes())
                .expect("the SVG should parse"),
        );
        let image = cx
            .svg_renderer()
            .render_parsed(&parsed, 1.0)
            .expect("the SVG should rasterize");
        (image, Arc::new(geometry))
    });
    markdown.update(cx, |markdown, _| {
        markdown.mermaid_state.cache.insert(
            contents.clone(),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(image),
                None,
                None,
                Some(geometry.clone()),
            )),
        );
        markdown.mermaid_state.order = vec![contents.clone()];
    });

    let rect = geometry
        .labels
        .iter()
        .find(|label| item(&label.item))
        .unwrap_or_else(|| panic!("the label should be mapped in {:?}", geometry.labels))
        .rect;
    let requests = Rc::new(RefCell::new(Vec::new()));
    let bounds = draw(cx, &markdown, &requests);
    let scale = f32::from(bounds.size.width) / geometry.view_box[2];
    let center = point(
        bounds.origin.x + px((rect[0] + rect[2] / 2.0 - geometry.view_box[0]) * scale),
        bounds.origin.y + px((rect[1] + rect[3] / 2.0 - geometry.view_box[1]) * scale),
    );
    cx.simulate_event(gpui::MouseDownEvent {
        position: center,
        button: gpui::MouseButton::Left,
        modifiers: Default::default(),
        click_count: 2,
        first_mouse: false,
    });
    let recorded = requests.borrow().clone();
    (recorded, geometry)
}

fn draw(
    cx: &mut VisualTestContext,
    markdown: &Entity<Markdown>,
    requests: &Rc<RefCell<Vec<Recorded>>>,
) -> gpui::Bounds<Pixels> {
    let markdown = markdown.clone();
    let requests = requests.clone();
    cx.draw(
        Default::default(),
        size(px(1600.0), px(900.0)),
        move |_, cx| {
            cx.new(|_| EditableMermaid { markdown, requests })
                .into_any_element()
        },
    );
    cx.debug_bounds("mermaid-label-hit-layer-0")
        .expect("the hit layer should have been painted over the raster")
}

fn expected(source: &str, token: &str) -> Option<Range<usize>> {
    let start = source.find(token).expect("the token is in the source");
    Some(start..start + token.len())
}

#[gpui::test]
fn test_double_clicking_a_pie_legend_entry_offers_its_label(cx: &mut TestAppContext) {
    let source = concat!(
        "```mermaid\n",
        "pie title \u{30da}\u{30c3}\u{30c8}\n",
        "  \"\u{72ac}\" : 386\n",
        "  \"\u{732b}\" : 85\n",
        "```",
    );
    let (requests, _) = double_click(cx, source, |item| *item == LabelItem::PieSlice { index: 1 });
    assert_eq!(
        requests,
        [(
            expected(source, "\"\u{732b}\""),
            "\"\u{732b}\"".to_string(),
            "\u{732b}".to_string(),
            true,
        )],
        "the second legend entry is the second slice, and `%%` cannot be written there"
    );
}

#[gpui::test]
fn test_double_clicking_a_gantt_task_offers_its_name(cx: &mut TestAppContext) {
    let source = concat!(
        "```mermaid\n",
        "gantt\n",
        "  title \u{8a08}\u{753b}\n",
        "  dateFormat YYYY-MM-DD\n",
        "  section \u{8a2d}\u{8a08}\n",
        "  \u{8981}\u{4ef6}\u{5b9a}\u{7fa9} :a1, 2024-01-01, 10d\n",
        "  \u{5b9f}\u{88c5} :after a1, 5d\n",
        "```",
    );
    let (requests, _) = double_click(cx, source, |item| {
        *item
            == LabelItem::GanttTask {
                task_id: "task1".into(),
            }
    });
    assert_eq!(
        requests,
        [(
            expected(source, "\u{5b9f}\u{88c5} :").map(|range| range.start..range.end - 2),
            "\u{5b9f}\u{88c5}".to_string(),
            "\u{5b9f}\u{88c5}".to_string(),
            true,
        )],
        "a task name opening with `%%` would turn the line into a comment"
    );

    let (requests, _) = double_click(cx, source, |item| {
        *item
            == LabelItem::GanttSection {
                name: "\u{8a2d}\u{8a08}".into(),
            }
    });
    assert_eq!(
        requests,
        [(
            expected(source, "section \u{8a2d}\u{8a08}")
                .map(|range| range.start + "section ".len()..range.end),
            "\u{8a2d}\u{8a08}".to_string(),
            "\u{8a2d}\u{8a08}".to_string(),
            false,
        )]
    );
}

/// The regression this whole path exists for: an attribute row used to open
/// the entity's alias, because the entity's box is all the geometry knew.
#[gpui::test]
fn test_double_clicking_an_er_attribute_offers_the_attribute_name(cx: &mut TestAppContext) {
    let source = concat!(
        "```mermaid\n",
        "erDiagram\n",
        "  \u{9867}\u{5ba2}[\"\u{304a}\u{5ba2}\u{69d8}\"] {\n",
        "    string \u{540d}\u{524d} PK\n",
        "    int \u{5e74}\u{9f62}\n",
        "  }\n",
        "```",
    );
    let (requests, geometry) = double_click(cx, source, |item| {
        *item
            == LabelItem::ErAttribute {
                entity: "\u{9867}\u{5ba2}".into(),
                index: 1,
            }
    });
    assert_eq!(
        requests,
        [(
            expected(source, "\u{5e74}\u{9f62}"),
            "\u{5e74}\u{9f62}".to_string(),
            "\u{5e74}\u{9f62}".to_string(),
            true,
        )],
        "the row's name, not the entity's alias, has to open"
    );
    // The entity itself is still reachable through its header.
    assert!(
        geometry
            .nodes
            .iter()
            .any(|node| node.node_id == "\u{9867}\u{5ba2}")
    );
}
