//! Geometry of class and sequence diagrams, read from what merman actually
//! draws, so that a merman release that respells the ids or the order these
//! tests pin turns them red instead of silently breaking the double-click.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::{DiagramGeometry, MermaidTheme, NodeGeometry, render_to_svg_with_geometry};

fn geometry_of(source: &str) -> (String, DiagramGeometry) {
    render_to_svg_with_geometry(source, &MermaidTheme::default())
        .expect("mermaid diagram should render to SVG")
}

fn inside(outer: [f32; 4], inner: [f32; 4]) -> bool {
    inner[0] >= outer[0] - 0.5
        && inner[1] >= outer[1] - 0.5
        && inner[0] + inner[2] <= outer[0] + outer[2] + 0.5
        && inner[1] + inner[3] <= outer[1] + outer[3] + 0.5
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

fn node<'a>(geometry: &'a DiagramGeometry, node_id: &str) -> &'a NodeGeometry {
    geometry
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .unwrap_or_else(|| panic!("node {node_id} should be mapped in {:?}", geometry.nodes))
}

/// A class is keyed by its name and labelled with its display name alone: the
/// annotation, members and methods drawn in the same box are not its label.
/// A relation names its ends in the order the statement wrote them, and
/// relations between the same pair are told apart by their position.
#[test]
fn class_boxes_and_relations_are_mapped() {
    let (svg, geometry) = geometry_of(concat!(
        "classDiagram\n",
        "  class Animal[\"\u{3069}\u{3046}\u{3076}\u{3064} #quot;x#quot;\"] {\n",
        "    <<interface>>\n",
        "    +String name\n",
        "    +eat()\n",
        "  }\n",
        "  Animal <|-- Duck : \u{7d99}\u{627f}\n",
        "  Animal <|-- Duck : two\n",
        "  Duck --> Pond\n",
        "  class List~T~\n",
        "  Duck ..> List~T~ : uses\n",
        "  note for Duck \"a note\"\n",
    ));

    let mut ids: Vec<&str> = geometry
        .nodes
        .iter()
        .map(|node| node.node_id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["Animal", "Duck", "List", "Pond"],
        "a note is drawn as a node but is not a class"
    );
    assert_eq!(
        node(&geometry, "Animal").label,
        "\u{3069}\u{3046}\u{3076}\u{3064} \"x\""
    );
    assert_eq!(node(&geometry, "Duck").label, "Duck");
    assert_eq!(node(&geometry, "List").label, "List<T>");
    for (index, first) in geometry.nodes.iter().enumerate() {
        assert!(
            inside(geometry.view_box, first.rect),
            "{} {:?} should be inside {:?}",
            first.node_id,
            first.rect,
            geometry.view_box
        );
        for second in &geometry.nodes[index + 1..] {
            assert!(
                !overlaps(first.rect, second.rect),
                "{} overlaps {}",
                first.node_id,
                second.node_id
            );
        }
        assert!(
            svg.contains(&format!("-classId-{}-", first.node_id)),
            "the id {} should survive the remaining passes",
            first.node_id
        );
    }

    let edges: Vec<_> = geometry
        .edges
        .iter()
        .map(|edge| {
            (
                edge.from.as_str(),
                edge.to.as_str(),
                edge.occurrence,
                edge.statement_index,
                edge.label.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        vec![
            ("Animal", "Duck", 0, None, Some("\u{7d99}\u{627f}")),
            ("Animal", "Duck", 1, None, Some("two")),
            ("Duck", "Pond", 0, None, None),
            ("Duck", "List", 0, None, Some("uses")),
        ],
        "the line to the note is not a relation"
    );
    for edge in &geometry.edges {
        assert_eq!(
            edge.label.is_some(),
            edge.label_rect.is_some(),
            "{} has a label without a box, or the other way round",
            edge.edge_id
        );
        if let Some(rect) = edge.label_rect {
            assert!(inside(geometry.view_box, rect), "{rect:?}");
        }
    }
}

/// A cardinality is drawn as a label of its own, outside the label group, and
/// must not be taken for the relation's label.
#[test]
fn a_cardinality_is_not_a_relation_label() {
    let (_, geometry) = geometry_of("classDiagram\n  Customer \"1\" --> \"*\" Ticket\n");
    assert_eq!(geometry.edges.len(), 1);
    assert_eq!(geometry.edges[0].label, None);
    assert_eq!(geometry.edges[0].label_rect, None);
}

fn items(geometry: &DiagramGeometry) -> Vec<(Option<usize>, Option<&str>)> {
    geometry
        .edges
        .iter()
        .map(|edge| (edge.statement_index, edge.label.as_deref()))
        .collect()
}

/// A sequence diagram numbers its messages and notes together, in the order
/// they were written — blocks, activations and a self message included — and
/// draws every participant twice.
#[test]
fn sequence_participants_messages_and_notes_are_mapped() {
    let (_, geometry) = geometry_of(concat!(
        "sequenceDiagram\n",
        "  participant A as \u{9867}\u{5ba2}\u{3055}\u{3093}\n",
        "  actor B\n",
        "  A->>B: \u{6ce8}\u{6587} #59; x\n",
        "  Note right of B: \u{78ba}\u{8a8d}<br>\u{4e2d}\n",
        "  loop \u{6bce}\u{65e5}\n",
        "    B-->>A: \u{5fdc}\u{7b54}\n",
        "  end\n",
        "  activate A\n",
        "  A->>A: self\n",
        "  Note over A,B: \u{7d42}\u{4e86}\n",
        "  A->>+B: \n",
    ));

    let participants: Vec<(&str, &str)> = geometry
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node.label.as_str()))
        .collect();
    assert_eq!(
        participants
            .iter()
            .filter(|(id, _)| *id == "A")
            .collect::<Vec<_>>(),
        vec![
            &("A", "\u{9867}\u{5ba2}\u{3055}\u{3093}"),
            &("A", "\u{9867}\u{5ba2}\u{3055}\u{3093}")
        ],
        "A is drawn at the top and at the bottom, with its display name"
    );
    assert_eq!(
        participants
            .iter()
            .filter(|(id, _)| *id == "B")
            .collect::<Vec<_>>(),
        vec![&("B", "B"), &("B", "B")],
        "the stick figure of an actor is a box too: {participants:?}"
    );
    assert_eq!(geometry.nodes.len(), 4);
    let a_boxes: Vec<[f32; 4]> = geometry
        .nodes
        .iter()
        .filter(|node| node.node_id == "A")
        .map(|node| node.rect)
        .collect();
    assert!(
        a_boxes[0][1] + a_boxes[0][3] <= a_boxes[1][1],
        "the top box comes first: {a_boxes:?}"
    );

    assert_eq!(
        items(&geometry),
        vec![
            (Some(0), Some("\u{6ce8}\u{6587} ; x")),
            (Some(1), Some("\u{78ba}\u{8a8d}\n\u{4e2d}")),
            (Some(2), Some("\u{5fdc}\u{7b54}")),
            (Some(3), Some("self")),
            (Some(4), Some("\u{7d42}\u{4e86}")),
            (Some(5), None),
        ]
    );
    let rects: Vec<[f32; 4]> = geometry
        .edges
        .iter()
        .map(|edge| {
            edge.label_rect
                .unwrap_or_else(|| panic!("{} should have a box", edge.edge_id))
        })
        .collect();
    for rect in &rects {
        assert!(
            inside(geometry.view_box, *rect),
            "{rect:?} should be inside {:?}",
            geometry.view_box
        );
        for participant in &geometry.nodes {
            assert!(
                !overlaps(*rect, participant.rect),
                "{rect:?} overlaps participant {} {:?}",
                participant.node_id,
                participant.rect
            );
        }
    }
    for pair in rects.windows(2) {
        assert!(
            pair[0][1] < pair[1][1],
            "each message and note sits below the one before it: {rects:?}"
        );
    }
}

fn collect(svg: &str) -> DiagramGeometry {
    let mut reader = Reader::from_str(svg);
    reader.config_mut().check_end_names = false;
    let mut input = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(event) => input.push(event.into_owned()),
            Err(error) => panic!("the fixture should parse: {error}"),
        }
    }
    let sink = Rc::new(RefCell::new(DiagramGeometry::default()));
    super::collect_geometry::process(input.into_iter().map(Ok), "m-0", sink.clone())
        .collect::<Result<Vec<_>>>()
        .expect("the fixture should walk");
    sink.take()
}

/// The numbering is only as good as every id it is built from: one message
/// whose number cannot be read would shift every message after it onto its
/// neighbour's statement, so none of them is offered.
#[test]
fn an_unreadable_message_number_drops_every_message() {
    let message = |id: &str, y: u32| {
        format!(
            r#"<text x="50" y="{y}" class="messageText" style="font-size:16px">m</text><line x1="0" y1="{y}" x2="100" y2="{y}" data-et="message" data-id="{id}"/>"#
        )
    };
    let svg = |ids: [&str; 2]| {
        format!(
            r#"<svg id="m-0" aria-roledescription="sequence" viewBox="0 0 200 200">{}{}</svg>"#,
            message(ids[0], 10),
            message(ids[1], 60)
        )
    };

    let readable = collect(&svg(["i3", "i0"]));
    let orders: Vec<(&str, Option<usize>)> = readable
        .edges
        .iter()
        .map(|edge| (edge.edge_id.as_str(), edge.statement_index))
        .collect();
    assert_eq!(
        orders,
        vec![("i0", Some(0)), ("i3", Some(1))],
        "merman's number orders the messages, not their place in the SVG"
    );

    assert!(collect(&svg(["i0", "x1"])).edges.is_empty());
    assert!(
        collect(&svg(["i1", "i1"])).edges.is_empty(),
        "two messages claiming one number cannot both be right"
    );
}

/// quick-xml hands an escaped `&` or `"` over as an event of its own, which the
/// labels of the kinds mapped before class and sequence diagrams hold too.
#[test]
fn escaped_characters_stay_in_every_kind_of_label() {
    let (_, flowchart) =
        geometry_of("flowchart LR\n  A[\"Tom & #quot;Jerry#quot;\"] -->|a & b| B\n");
    assert_eq!(node(&flowchart, "A").label, "Tom & \"Jerry\"");
    assert_eq!(
        flowchart
            .edges
            .first()
            .and_then(|edge| edge.label.as_deref()),
        Some("a & b")
    );

    let (_, state) = geometry_of("stateDiagram-v2\n  s1 : x & y\n  s1 --> s2 : p & q\n");
    assert_eq!(node(&state, "s1").label, "x & y");
    assert_eq!(
        state.edges.first().and_then(|edge| edge.label.as_deref()),
        Some("p & q")
    );

    let (_, er) = geometry_of("erDiagram\n  CUSTOMER ||--o{ ORDER : \"buys & sells\"\n");
    assert_eq!(
        er.edges.first().and_then(|edge| edge.label.as_deref()),
        Some("buys & sells")
    );
}
