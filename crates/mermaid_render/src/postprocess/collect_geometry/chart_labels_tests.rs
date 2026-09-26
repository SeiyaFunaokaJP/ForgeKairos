//! The labels of pie charts, gantt charts and ER attribute cells, read from
//! the SVG merman actually produces.

use crate::{DiagramGeometry, LabelGeometry, LabelItem, MermaidTheme, render_to_svg_with_geometry};

fn geometry_of(source: &str) -> DiagramGeometry {
    let (_, geometry) = render_to_svg_with_geometry(source, &MermaidTheme::default())
        .unwrap_or_else(|error| panic!("{source:?} should render: {error:#}"));
    geometry
}

fn label<'a>(geometry: &'a DiagramGeometry, item: &LabelItem) -> &'a LabelGeometry {
    geometry
        .labels
        .iter()
        .find(|label| &label.item == item)
        .unwrap_or_else(|| panic!("{item:?} should be mapped in {:?}", geometry.labels))
}

fn contains(outer: [f32; 4], inner: [f32; 4]) -> bool {
    inner[0] >= outer[0] - 0.5
        && inner[1] >= outer[1] - 0.5
        && inner[0] + inner[2] <= outer[0] + outer[2] + 0.5
        && inner[1] + inner[3] <= outer[1] + outer[3] + 0.5
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

fn assert_disjoint(labels: &[&LabelGeometry]) {
    for (index, first) in labels.iter().enumerate() {
        for second in &labels[index + 1..] {
            assert!(
                !overlaps(first.rect, second.rect),
                "{first:?} overlaps {second:?}"
            );
        }
    }
}

/// A legend entry is the slice at its position among the ones merman keeps:
/// the second `犬` is dropped, and a slice too small to draw a wedge for still
/// has its entry.
#[test]
fn pie_legend_entries_are_numbered_like_the_slices_merman_keeps() {
    let geometry = geometry_of(concat!(
        "pie showData title \u{30da}\u{30c3}\u{30c8}\n",
        "  \"\u{72ac}\" : 386\n",
        "  \"\u{732b}\" : 85\n",
        "  \"\u{72ac}\" : 3\n",
        "  \"\u{30cd}\u{30ba}\u{30df}\" : 0.1\n",
    ));
    assert!(geometry.nodes.is_empty() && geometry.edges.is_empty());
    let mapped: Vec<(&LabelItem, &str)> = geometry
        .labels
        .iter()
        .map(|label| (&label.item, label.label.as_str()))
        .collect();
    assert_eq!(
        mapped,
        vec![
            (&LabelItem::Title, "\u{30da}\u{30c3}\u{30c8}"),
            (&LabelItem::PieSlice { index: 0 }, "\u{72ac} [386]"),
            (&LabelItem::PieSlice { index: 1 }, "\u{732b} [85]"),
            (
                &LabelItem::PieSlice { index: 2 },
                "\u{30cd}\u{30ba}\u{30df} [0.1]"
            ),
        ]
    );
    for label in &geometry.labels {
        assert!(
            contains(geometry.view_box, label.rect),
            "{label:?} should be inside {:?}",
            geometry.view_box
        );
    }
    let entries: Vec<&LabelGeometry> = geometry
        .labels
        .iter()
        .filter(|label| matches!(label.item, LabelItem::PieSlice { .. }))
        .collect();
    assert_disjoint(&entries);
    // Each entry starts at its colour swatch, which sits right of the pie
    // (centred at 225, radius 185) and stacks downwards in source order.
    for pair in entries.windows(2) {
        assert!(pair[0].rect[1] < pair[1].rect[1], "{pair:?}");
    }
    for entry in &entries {
        assert!(entry.rect[0] > 225.0 + 185.0 - 1.0, "{entry:?}");
        assert!(entry.rect[3] >= 18.0, "{entry:?} should cover its swatch");
    }
    // The title is centred above the pie.
    let title = label(&geometry, &LabelItem::Title);
    assert!(title.rect[0] < 225.0 && title.rect[0] + title.rect[2] > 225.0);
    assert!(title.rect[1] + title.rect[3] <= 225.0 - 185.0 + 1.0);

    // Without a title there is nothing to map for it.
    let untitled = geometry_of("pie\n  \"a\" : 1\n");
    assert!(
        untitled
            .labels
            .iter()
            .all(|label| label.item != LabelItem::Title)
    );
}

const GANTT: &str = concat!(
    "gantt\n",
    "  title \u{8a08}\u{753b}\n",
    "  dateFormat YYYY-MM-DD\n",
    "  section \u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}\n",
    "  \u{8981}\u{4ef6}\u{5b9a}\u{7fa9} :a1, 2024-01-01, 10d\n",
    "  \u{9577}\u{3044}\u{540d}\u{524d}\u{306e}\u{30bf}\u{30b9}\u{30af}\u{3067}\u{3059} :after a1, 3d\n",
    "  section \u{5b9f}\u{88c5}\n",
    "  \u{4f5c}\u{308b} :2024-01-20, 5d\n",
    "  \u{30de}\u{30a4}\u{30eb} :milestone, m1, 2024-01-25, 0d\n",
);

#[test]
fn gantt_tasks_sections_and_title_are_mapped() {
    let geometry = geometry_of(GANTT);
    assert!(geometry.nodes.is_empty() && geometry.edges.is_empty());
    let task = |task_id: &str| {
        label(
            &geometry,
            &LabelItem::GanttTask {
                task_id: task_id.into(),
            },
        )
    };
    assert_eq!(task("a1").label.trim(), "\u{8981}\u{4ef6}\u{5b9a}\u{7fa9}");
    assert_eq!(
        task("task1").label.trim(),
        "\u{9577}\u{3044}\u{540d}\u{524d}\u{306e}\u{30bf}\u{30b9}\u{30af}\u{3067}\u{3059}"
    );
    assert_eq!(task("task2").label.trim(), "\u{4f5c}\u{308b}");
    assert_eq!(task("m1").label.trim(), "\u{30de}\u{30a4}\u{30eb}");
    // Each task covers its bar, which merman draws at `y = 50 + 24 * row`.
    for (index, task_id) in ["a1", "task1", "task2", "m1"].iter().enumerate() {
        let rect = task(task_id).rect;
        let bar_top = 50.0 + 24.0 * index as f32;
        assert!(
            rect[1] <= bar_top && rect[1] + rect[3] >= bar_top + 20.0,
            "{task_id} {rect:?} should cover its bar"
        );
    }
    // A milestone's name is drawn left of its diamond, and has to be reachable
    // there too.
    assert!(task("m1").rect[0] < 1099.0 - 20.0, "{:?}", task("m1"));
    let tasks: Vec<&LabelGeometry> = ["a1", "task1", "task2", "m1"]
        .iter()
        .map(|task_id| task(task_id))
        .collect();
    assert_disjoint(&tasks);

    let first = label(
        &geometry,
        &LabelItem::GanttSection {
            name: "\u{8a2d}\u{8a08}\n\u{5de5}\u{7a0b}".into(),
        },
    );
    assert_eq!(first.label, "\u{8a2d}\u{8a08}\n\u{5de5}\u{7a0b}");
    let second = label(
        &geometry,
        &LabelItem::GanttSection {
            name: "\u{5b9f}\u{88c5}".into(),
        },
    );
    // The first section spans rows 0 and 1 (y 48 to 96), the second rows 2
    // and 3.
    assert!(first.rect[1] >= 48.0 - 4.0 && first.rect[1] + first.rect[3] <= 96.0 + 4.0);
    assert!(second.rect[1] >= 96.0 - 4.0 && second.rect[1] + second.rect[3] <= 144.0 + 4.0);
    assert!(
        first.rect[3] > second.rect[3],
        "two lines are taller than one"
    );
    assert_disjoint(&[first, second]);

    assert_eq!(
        label(&geometry, &LabelItem::Title).label,
        "\u{8a08}\u{753b}"
    );
    for label in &geometry.labels {
        assert!(
            contains(geometry.view_box, label.rect),
            "{label:?} should be inside {:?}",
            geometry.view_box
        );
    }
}

/// Two tasks with the same id are drawn under the same element id, so neither
/// can be told apart from the other.
#[test]
fn gantt_tasks_sharing_an_id_are_not_mapped() {
    let geometry = geometry_of(concat!(
        "gantt\n",
        "  dateFormat YYYY-MM-DD\n",
        "  x :t, 2024-01-01, 1d\n",
        "  y :t, 2024-01-02, 1d\n",
        "  z :2024-01-03, 1d\n",
    ));
    let tasks: Vec<&LabelItem> = geometry
        .labels
        .iter()
        .map(|label| &label.item)
        .filter(|item| matches!(item, LabelItem::GanttTask { .. }))
        .collect();
    assert_eq!(
        tasks,
        vec![&LabelItem::GanttTask {
            task_id: "task1".into()
        }]
    );
}

/// Each attribute's name cell is a target of its own inside its entity, and the
/// entity itself stays mapped as before.
#[test]
fn er_attribute_names_are_mapped_inside_their_entity() {
    let geometry = geometry_of(concat!(
        "erDiagram\n",
        "  \u{9867}\u{5ba2}[\u{304a}\u{5ba2}\u{69d8}] {\n",
        "    string \u{540d}\u{524d} PK \"\u{6c0f}\u{540d}\"\n",
        "    int age\n",
        "  }\n",
        "  \u{6ce8}\u{6587} {\n",
        "    int id PK\n",
        "  }\n",
        "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : places\n",
    ));
    let entity = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "\u{9867}\u{5ba2}")
        .expect("the entity is still mapped");
    assert_eq!(entity.label, "\u{304a}\u{5ba2}\u{69d8}");
    let order = geometry
        .nodes
        .iter()
        .find(|node| node.node_id == "\u{6ce8}\u{6587}")
        .expect("the entity is still mapped");

    let attribute = |entity: &str, index| {
        label(
            &geometry,
            &LabelItem::ErAttribute {
                entity: entity.into(),
                index,
            },
        )
    };
    let name = attribute("\u{9867}\u{5ba2}", 0);
    let age = attribute("\u{9867}\u{5ba2}", 1);
    let id = attribute("\u{6ce8}\u{6587}", 0);
    assert_eq!(name.label, "\u{540d}\u{524d}");
    assert_eq!(age.label, "age");
    assert_eq!(id.label, "id");
    assert_eq!(geometry.labels.len(), 3, "{:?}", geometry.labels);

    for cell in [name, age] {
        assert!(contains(entity.rect, cell.rect), "{cell:?} in {entity:?}");
        // The name column only: the type left of it and the keys and
        // comment right of it stay outside.
        assert!(
            cell.rect[2] < entity.rect[2] / 2.0,
            "{cell:?} in {entity:?}"
        );
        assert!(
            cell.rect[0] > entity.rect[0] + 20.0,
            "{cell:?} in {entity:?}"
        );
    }
    assert!(contains(order.rect, id.rect), "{id:?} in {order:?}");
    assert!(name.rect[1] + name.rect[3] <= age.rect[1] + 0.5);
    assert_disjoint(&[name, age, id]);
}
