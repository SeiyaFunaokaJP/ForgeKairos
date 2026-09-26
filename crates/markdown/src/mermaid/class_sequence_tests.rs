//! The double-click path for class and sequence diagrams, driven through the
//! geometry merman actually produces for them.

use super::CachedMermaidDiagram;
use crate::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
    MarkdownStyle, WrapButtonVisibility,
};
use gpui::{Context, Entity, IntoElement, Render, TestAppContext, Window, point, size};
use mermaid_render::DiagramGeometry;
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use ui::prelude::*;

type Recorded = (Option<Range<usize>>, String, String, &'static str);

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
                        request.refusal_message(),
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

/// Renders the one fence of `source` with merman's own raster and geometry,
/// and returns a way to double-click a `viewBox` rectangle's center along
/// with the requests those clicks raised.
struct Harness<'a> {
    cx: &'a mut gpui::VisualTestContext,
    markdown: Entity<Markdown>,
    geometry: Arc<DiagramGeometry>,
    requests: Rc<RefCell<Vec<Recorded>>>,
}

impl<'a> Harness<'a> {
    fn new(source: &str, cx: &'a mut TestAppContext) -> Self {
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
        Self {
            cx,
            markdown,
            geometry,
            requests: Rc::default(),
        }
    }

    fn double_click(&mut self, rect: [f32; 4]) -> Vec<Recorded> {
        self.requests.borrow_mut().clear();
        let markdown = self.markdown.clone();
        let requests = self.requests.clone();
        self.cx.draw(
            Default::default(),
            size(px(1600.0), px(1600.0)),
            move |_, cx| {
                cx.new(|_| EditableMermaid { markdown, requests })
                    .into_any_element()
            },
        );
        let bounds = self
            .cx
            .debug_bounds("mermaid-label-hit-layer-0")
            .expect("the hit layer should have been painted over the raster");
        let view_box = self.geometry.view_box;
        let scale = f32::from(bounds.size.width) / view_box[2];
        let position = point(
            bounds.origin.x + px((rect[0] + rect[2] / 2.0 - view_box[0]) * scale),
            bounds.origin.y + px((rect[1] + rect[3] / 2.0 - view_box[1]) * scale),
        );
        self.cx.simulate_event(gpui::MouseDownEvent {
            position,
            button: gpui::MouseButton::Left,
            modifiers: Default::default(),
            click_count: 2,
            first_mouse: false,
        });
        self.requests.borrow().clone()
    }
}

const NO_LABEL_TOKEN: &str = "この要素は ID がそのまま図に出ています。表示名を変えるには「Mermaid 図を編集」から説明や別名を付けてください。";
const NOT_LOCATED: &str = "このラベルはソースの位置を特定できませんでした。";

fn located(source: &str, token: &str) -> Recorded {
    let start = source.find(token).expect("the token is in the source");
    (
        Some(start..start + token.len()),
        token.to_string(),
        token.to_string(),
        NOT_LOCATED,
    )
}

#[gpui::test]
fn test_double_clicking_sequence_labels_reaches_the_callback(cx: &mut TestAppContext) {
    let source = concat!(
        "```mermaid\n",
        "sequenceDiagram\n",
        "    participant A as \u{9867}\u{5ba2}\n",
        "    participant B\n",
        "    A->>B: \u{6ce8}\u{6587}\u{3059}\u{308b}\n",
        "    Note right of B: \u{78ba}\u{8a8d}\n",
        "    B-->>A: \u{5fdc}\u{7b54}\n",
        "```",
    );
    let mut harness = Harness::new(source, cx);
    let geometry = harness.geometry.clone();

    let boxes: Vec<[f32; 4]> = geometry
        .nodes
        .iter()
        .filter(|node| node.node_id == "A")
        .map(|node| node.rect)
        .collect();
    assert_eq!(boxes.len(), 2, "A is drawn at the top and at the bottom");
    for rect in boxes {
        assert_eq!(
            harness.double_click(rect),
            [located(source, "\u{9867}\u{5ba2}")],
            "either box of A offers its display name"
        );
    }

    let b = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "B")
        .expect("B should be mapped");
    assert_eq!(
        harness.double_click(b.rect),
        [(None, String::new(), "B".to_string(), NO_LABEL_TOKEN)],
        "a participant drawn with its id has no display name to rewrite"
    );

    for (index, token) in [
        "\u{6ce8}\u{6587}\u{3059}\u{308b}",
        "\u{78ba}\u{8a8d}",
        "\u{5fdc}\u{7b54}",
    ]
    .into_iter()
    .enumerate()
    {
        let rect = geometry
            .edges
            .iter()
            .find(|edge| edge.statement_index == Some(index))
            .and_then(|edge| edge.label_rect)
            .unwrap_or_else(|| panic!("item {index} should be mapped: {:?}", geometry.edges));
        assert_eq!(harness.double_click(rect), [located(source, token)]);
    }
}

#[gpui::test]
fn test_double_clicking_class_labels_reaches_the_callback(cx: &mut TestAppContext) {
    let source = concat!(
        "```mermaid\n",
        "classDiagram\n",
        "    class Animal[\"\u{52d5}\u{7269}\"]\n",
        "    Animal <|-- Duck : \u{7d99}\u{627f}\n",
        "```",
    );
    let mut harness = Harness::new(source, cx);
    let geometry = harness.geometry.clone();

    let animal = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "Animal")
        .expect("Animal should be mapped");
    let start = source.find("\"\u{52d5}\u{7269}\"").expect("in the source");
    let quoted = "\"\u{52d5}\u{7269}\"";
    assert_eq!(
        harness.double_click(animal.rect),
        [(
            Some(start..start + quoted.len()),
            quoted.to_string(),
            "\u{52d5}\u{7269}".to_string(),
            NOT_LOCATED
        )]
    );

    let duck = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "Duck")
        .expect("Duck should be mapped");
    assert_eq!(
        harness.double_click(duck.rect),
        [(None, String::new(), "Duck".to_string(), NO_LABEL_TOKEN)]
    );

    let relation = geometry
        .edges
        .first()
        .and_then(|edge| edge.label_rect)
        .expect("the relation label should be mapped");
    assert_eq!(
        harness.double_click(relation),
        [located(source, "\u{7d99}\u{627f}")]
    );
}

/// A participant is drawn twice, and the box under the pointer is the one that
/// has to light up and anchor the field, not the first box with that id.
#[test]
fn the_second_box_of_a_participant_is_a_target_of_its_own() {
    use super::{MermaidLabelTarget, mermaid_label_at, mermaid_label_rect, mermaid_rendered_label};
    use mermaid_render::NodeGeometry;

    let geometry = DiagramGeometry {
        view_box: [0.0, 0.0, 100.0, 200.0],
        nodes: vec![
            NodeGeometry {
                node_id: "A".into(),
                label: "top".into(),
                rect: [0.0, 0.0, 50.0, 20.0],
            },
            NodeGeometry {
                node_id: "A".into(),
                label: "bottom".into(),
                rect: [0.0, 180.0, 50.0, 20.0],
            },
        ],
        edges: Vec::new(),
        labels: Vec::new(),
    };
    assert_eq!(
        mermaid_label_at(&geometry, point(10.0, 10.0)),
        Some(MermaidLabelTarget::Node {
            node_id: "A".into()
        })
    );
    let bottom = mermaid_label_at(&geometry, point(10.0, 190.0)).expect("the bottom box is hit");
    assert_eq!(
        bottom,
        MermaidLabelTarget::NodeCopy {
            node_id: "A".into(),
            copy: 1
        }
    );
    assert_eq!(
        mermaid_label_rect(&geometry, &bottom),
        Some([0.0, 180.0, 50.0, 20.0])
    );
    assert_eq!(mermaid_rendered_label(&geometry, &bottom), "bottom");
}

/// What the field writes has to come back out of merman as exactly what was
/// typed, characters the statement would otherwise read as syntax included.
#[test]
fn written_labels_render_as_typed() {
    use crate::mermaid_source::{find_edge_label, find_node_label, find_transition_label};

    let theme = mermaid_render::MermaidTheme::default();
    let write = |body: &str, span: crate::mermaid_source::MermaidLabelSpan, typed: &str| {
        let mut edited = body.to_string();
        let encoded =
            crate::mermaid_source::encode_label(typed, span.quoting, &body[span.range.clone()]);
        edited.replace_range(span.range, &encoded);
        edited
    };

    let sequence = "sequenceDiagram\n    participant A as old\n    A->>B: old\n    B-->>A: next\n";
    let typed = "a;b #1 50%% c";
    let span = find_transition_label(sequence, 0).expect("the message is found");
    let edited = write(sequence, span, typed);
    let (_, geometry) =
        mermaid_render::render_to_svg_with_geometry(&edited, &theme).expect("still renders");
    let labels: Vec<Option<&str>> = geometry
        .edges
        .iter()
        .map(|edge| edge.label.as_deref())
        .collect();
    assert_eq!(labels, vec![Some(typed), Some("next")], "{edited}");
    let span = find_node_label(&edited, "A").expect("the alias is found");
    let renamed = write(&edited, span, "\u{9867}\u{5ba2} #2");
    let (_, geometry) =
        mermaid_render::render_to_svg_with_geometry(&renamed, &theme).expect("still renders");
    assert!(
        geometry
            .nodes
            .iter()
            .filter(|node| node.node_id == "A")
            .all(|node| node.label == "\u{9867}\u{5ba2} #2"),
        "{renamed}: {:?}",
        geometry.nodes
    );

    let class = "classDiagram\n    class A[\"old\"]\n    A --> B : old\n";
    let span = find_node_label(class, "A").expect("the display name is found");
    let edited = write(class, span, "say \"hi\"");
    let span = find_edge_label(&edited, "A", "B", 0).expect("the relation label is found");
    let edited = write(&edited, span, "\u{95a2}\u{9023} #3");
    let (_, geometry) =
        mermaid_render::render_to_svg_with_geometry(&edited, &theme).expect("still renders");
    let a = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "A")
        .expect("A is mapped");
    assert_eq!(a.label, "say \"hi\"", "{edited}");
    assert_eq!(
        geometry
            .edges
            .first()
            .and_then(|edge| edge.label.as_deref()),
        Some("\u{95a2}\u{9023} #3"),
        "{edited}"
    );
}

#[test]
fn labels_that_cannot_be_written_are_refused_with_a_reason() {
    use crate::mermaid_source::{MermaidLabelQuoting, write_refusal};

    assert!(write_refusal(MermaidLabelQuoting::ClassRelationLabel, "50%% off").is_some());
    assert!(write_refusal(MermaidLabelQuoting::ClassRelationLabel, "50% off").is_none());
    assert!(write_refusal(MermaidLabelQuoting::SequenceText, "wrap: x").is_some());
    assert!(write_refusal(MermaidLabelQuoting::SequenceText, "Nowrap:x").is_some());
    assert!(write_refusal(MermaidLabelQuoting::SequenceText, "  ").is_some());
    assert!(write_refusal(MermaidLabelQuoting::SequenceText, "wrapped: x").is_none());
    assert!(write_refusal(MermaidLabelQuoting::ClassLabel, "\"").is_none());
}

/// Every message and note merman numbers has to be the statement the source
/// scan numbers the same, however the blocks, activations and meta statements
/// around them are mixed: a mismatch rewrites another statement's text.
fn assert_sequence_items_match_the_source(fence_body: &str) {
    use crate::mermaid_source::find_transition_label;

    let (_, geometry) = mermaid_render::render_to_svg_with_geometry(
        fence_body,
        &mermaid_render::MermaidTheme::default(),
    )
    .expect("the diagram should render");
    assert!(
        !geometry.edges.is_empty(),
        "no items were mapped: {fence_body}"
    );
    for edge in &geometry.edges {
        let index = edge.statement_index.expect("every item is numbered");
        let found = find_transition_label(fence_body, index).unwrap_or_else(|refusal| {
            panic!("item {index} was refused: {refusal:?}\n{fence_body}")
        });
        assert_eq!(
            Some(found.label.as_str()),
            edge.label.as_deref(),
            "item {index} ({}) is not the statement merman drew\n{fence_body}",
            &fence_body[found.range]
        );
    }
    assert!(
        find_transition_label(fence_body, geometry.edges.len()).is_err(),
        "the source numbers more items than merman drew\n{fence_body}"
    );
}

#[test]
fn sequence_numbering_matches_merman_across_every_block_kind() {
    assert_sequence_items_match_the_source(concat!(
        "sequenceDiagram\n",
        "    autonumber\n",
        "    box Aqua \u{9867}\u{5ba2}\u{5074}\n",
        "    participant A as \u{9867}\u{5ba2}\n",
        "    actor U as \u{5229}\u{7528}\u{8005}\n",
        "    end\n",
        "    participant B\n",
        "    create participant C as \u{65b0}\u{898f}\n",
        "    A->>C: \u{4f5c}\u{6210}\n",
        "    A->>+B: \u{6ce8}\u{6587}; B-->>-A: \u{53d7}\u{4ed8} #59; \u{6e08}\n",
        "    activate A\n",
        "    loop \u{6bce}\u{65e5}\n",
        "        B->>U: \u{901a}\u{77e5} %% memo\n",
        "    end\n",
        "    alt \u{6210}\u{529f}\n",
        "        U->>B: OK\n",
        "    else \u{5931}\u{6557}\n",
        "        U-xB: NG\n",
        "    end\n",
        "    par \u{4e26}\u{884c}\n",
        "        A-)B: p1\n",
        "    and \u{5225}\n",
        "        A--)U: p2\n",
        "    end\n",
        "    rect rgb(0,0,0)\n",
        "        Note over A,B: \u{67a0}\u{5185}\n",
        "    end\n",
        "    critical \u{91cd}\u{8981}\n",
        "        A->>B: c1\n",
        "    option \u{4ee3}\u{66ff}\n",
        "        A->>B: c2\n",
        "    end\n",
        "    break \u{4e2d}\u{65ad}\n",
        "        B->>A: b1\n",
        "    end\n",
        "    opt \u{4efb}\u{610f}\n",
        "        A->>A: \u{81ea}\u{5df1}\n",
        "    end\n",
        "    deactivate A\n",
        "    links A: {\"Dash\": \"https://example.com\"}\n",
        "    destroy C\n",
        "    A->>C: \u{7834}\u{68c4}\n",
        "    Note right of A: \u{6700}\u{5f8c}\n",
    ));
}

/// merman reads an id followed by an arrow as a message before it looks for
/// a keyword, so a participant named like one still sends messages.
#[test]
fn sequence_numbering_matches_merman_for_participants_named_like_keywords() {
    assert_sequence_items_match_the_source(concat!(
        "sequenceDiagram\n",
        "    participant Box\n",
        "    participant Note\n",
        "    participant End\n",
        "    Box->>End: one\n",
        "    Note ->> Box: two\n",
        "    End->>Note: three\n",
        "    loop->>Box: four\n",
        "    Note over Box: five\n",
    ));
    assert_sequence_items_match_the_source(concat!(
        "sequenceDiagram\n",
        "    participant Box\n",
        "    Box->>A: one\n",
        "    A->>Box: two\n",
        "    Alt->>Box: three\n",
    ));
}

/// A relation between the same pair is told apart by its position among the
/// relations of that pair, written in either direction, and a class declared
/// twice or inside a namespace keeps its id.
#[test]
fn class_labels_match_merman_for_repeated_and_reversed_relations() {
    use crate::mermaid_source::{find_edge_label, find_node_label};

    let fence_body = concat!(
        "classDiagram\n",
        "    namespace \u{52d5}\u{7269} {\n",
        "        class Animal[\"\u{52d5}\u{7269}\"]\n",
        "        class Duck\n",
        "    }\n",
        "    class Animal[\"\u{518d}\u{5ba3}\u{8a00}\"]\n",
        "    class Animal\n",
        "    class Pond\n",
        "    Animal <|-- Duck : \u{7d99}\u{627f}\n",
        "    Duck --> Pond : \u{6cf3}\u{3050}\n",
        "    Animal <|-- Duck : \u{4e8c}\u{672c}\u{76ee}\n",
        "    Duck <-- Animal : \u{9006}\u{5411}\u{304d}\n",
        "    Pond \"1\" --> \"*\" Duck : \u{4f4f}\u{3080}\n",
        "    Animal <|-- Duck : \u{4e09}\u{672c}\u{76ee}\n",
    );
    let (_, geometry) = mermaid_render::render_to_svg_with_geometry(
        fence_body,
        &mermaid_render::MermaidTheme::default(),
    )
    .expect("the diagram should render");
    assert_eq!(geometry.edges.len(), 6, "{:?}", geometry.edges);
    for edge in &geometry.edges {
        let found = find_edge_label(fence_body, &edge.from, &edge.to, edge.occurrence)
            .unwrap_or_else(|refusal| panic!("{} was refused: {refusal:?}", edge.edge_id));
        assert_eq!(
            Some(found.label.as_str()),
            edge.label.as_deref(),
            "{} is not the relation merman drew",
            edge.edge_id
        );
    }
    let animal = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "Animal")
        .expect("Animal is mapped");
    let found = find_node_label(fence_body, "Animal").expect("Animal has a display name");
    assert_eq!(found.label, animal.label);
    assert_eq!(found.label, "\u{518d}\u{5ba3}\u{8a00}");
}
