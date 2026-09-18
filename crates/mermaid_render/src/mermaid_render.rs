// for a very big json! macro
#![recursion_limit = "256"]

//! Crate for rendering Mermaid diagram strings to SVG strings.
//!
//! The entrypoint to this crate is [`render_to_svg`].
//!
//! It takes a `&str` and a [`MermaidTheme`]. The output is an SVG with the
//! following properties:
//! - The style matches the provided theme
//! - Nodes are given accent colors, even if none are provided in the mermaid
//!   source.
//! - The SVG has been tweaked based on the assumption that it will be rasterized
//!   using `usvg`/`resvg`. Some bugs/quirks of `usvg`/`resvg` are accounted for
//!   in this crate.
//!
//! This module uses the [`merman`] crate for rendering, rather than
//! `mermaid-rs`, which was used in the previous implementation of mermaid
//! rendering in Zed.
//!
//! Historically, this crate also carried generic `usvg`/`resvg` cleanup for SVG
//! constructs that merman's parity output could emit, such as HTML labels in
//! `<foreignObject>` and CSS/attribute forms that rasterizers do not handle.
//! Since merman 0.6, that generic cleanup is exposed as merman's raster-safe SVG
//! pipeline. Zed opts into that pipeline during rendering, then keeps
//! editor-specific theme and accent color rules in this crate. The [`gpui`]
//! dependency is only needed for the [`Hsla`] and [`Rgba`] color types.
//!
//! The [`render_to_svg`] function operates in two stages:
//! - [`render`] the mermaid text to raster-safe SVG using [`merman`].
//! - [`postprocess`] the SVG to add Zed theme and accent styling.
//!
//! Zed's postprocessing is split up into stages. We parse the generated SVG
//! using [`quick_xml`], which produces an iterator of
//! [`Event<'_>`](quick_xml::events::Event)s. This iterator is then repeatedly
//! transformed, and finally collected back into an SVG string.
//!
//! This approach:
//! - Avoids doing multiple expensive string insertions.
//! - Avoids parsing the SVG multiple times (without needing to put all the
//!   logic in one huge function).
//! - But is quite a bit more complex.
//!
//! I think this complexity is justified because of the drastic performance
//! impact, as well as the low-risk nature; this code cannot panic, and errors
//! in the output just produce weird-looking diagrams.
//!
//! ## Color handling
//!
//! We try to match the users theme, and also apply accent colors to diagrams to
//! make them more visually interesting. Accent colors are derived from the
//! `player_colors` in the Zed theme.
//!
//! There are three parts to color handling:
//!
//! 1. A [`merman::MermaidConfig`] is passed when initially rendering the
//!    diagram. This sets most "normal" colors (background, text, etc.). However,
//!    it's not possible to color nodes individually, and not all parts of the
//!    diagrams are correctly themed.
//! 2. `postprocess::accent_colors` injects custom CSS classes (e.g.
//!    `zed-accent-0`) to specific elements, based on the diagram type and
//!    node.
//! 3. `postprocess::inject_css` injects CSS rules for the classes applied by
//!    `accent_colors`

pub mod class_model;
pub mod er_model;
pub mod flowchart_model;
pub mod gantt_model;
pub mod mermaid_model;
pub mod pie_model;
mod postprocess;
mod render;
pub mod sequence_model;
pub mod state_model;

use anyhow::Result;
use gpui::{Hsla, Rgba};

#[derive(Debug, Clone, Copy)]
pub struct AccentColor {
    pub foreground: Hsla,
    pub background: Hsla,
}

#[derive(Debug, Clone)]
pub struct MermaidTheme {
    pub dark_mode: bool,
    pub font_family: String,
    pub background: Hsla,
    pub primary_color: Hsla,
    pub primary_text_color: Hsla,
    pub primary_border_color: Hsla,
    pub secondary_color: Hsla,
    pub tertiary_color: Hsla,
    pub line_color: Hsla,
    pub text_color: Hsla,
    pub edge_label_background: Hsla,
    pub cluster_background: Hsla,
    pub cluster_border: Hsla,
    pub note_background: Hsla,
    pub note_border: Hsla,
    pub actor_background: Hsla,
    pub actor_border: Hsla,
    pub activation_background: Hsla,
    pub activation_border: Hsla,
    pub git_branch_colors: [Hsla; 8],
    pub git_branch_label_colors: [Hsla; 8],
    pub er_attr_bg_odd: Hsla,
    pub er_attr_bg_even: Hsla,
    pub error_color: Hsla,
    pub warning_color: Hsla,
    pub accent_colors: Vec<AccentColor>,
}

/// Default theme for testing.
#[cfg(any(test, feature = "test-support"))]
impl Default for MermaidTheme {
    fn default() -> Self {
        use gpui::{hsla, rgb};
        let git_branch_colors: [Hsla; 8] = [
            hsla(240.0 / 360.0, 1.0, 0.462_745_1, 1.0),
            hsla(60.0 / 360.0, 1.0, 0.435_294_12, 1.0),
            hsla(80.0 / 360.0, 1.0, 0.462_745_1, 1.0),
            hsla(210.0 / 360.0, 1.0, 0.462_745_1, 1.0),
            hsla(180.0 / 360.0, 1.0, 0.462_745_1, 1.0),
            hsla(150.0 / 360.0, 1.0, 0.462_745_1, 1.0),
            hsla(300.0 / 360.0, 1.0, 0.462_745_1, 1.0),
            hsla(0.0, 1.0, 0.462_745_1, 1.0),
        ];
        let git_branch_label_colors: [Hsla; 8] =
            git_branch_colors.map(crate::text_color_for_background);

        Self {
            dark_mode: false,
            font_family: "Inter, ui-sans-serif, system-ui, -apple-system, \"Segoe UI\", \"DejaVu Sans\", \"Liberation Sans\", sans-serif, \"Noto Color Emoji\", \"Apple Color Emoji\", \"Segoe UI Emoji\"".to_string(),
            background: rgb(0xFFFFFF).into(),
            primary_color: rgb(0xF8FAFC).into(),
            primary_text_color: rgb(0x0F172A).into(),
            primary_border_color: rgb(0x94A3B8).into(),
            secondary_color: rgb(0xE2E8F0).into(),
            tertiary_color: rgb(0xFFFFFF).into(),
            line_color: rgb(0x64748B).into(),
            text_color: rgb(0x0F172A).into(),
            edge_label_background: rgb(0xFFFFFF).into(),
            cluster_background: rgb(0xF1F5F9).into(),
            cluster_border: rgb(0xCBD5E1).into(),
            note_background: rgb(0xFFF7ED).into(),
            note_border: rgb(0xFDBA74).into(),
            actor_background: rgb(0xF8FAFC).into(),
            actor_border: rgb(0x94A3B8).into(),
            activation_background: rgb(0xE2E8F0).into(),
            activation_border: rgb(0x94A3B8).into(),
            git_branch_colors,
            git_branch_label_colors,
            er_attr_bg_odd: rgb(0x94A3B8).into(),
            er_attr_bg_even: rgb(0x0F172A).into(),
            error_color: rgb(0xDC2626).into(),
            warning_color: rgb(0xD97706).into(),
            accent_colors: Vec::new(),
        }
    }
}

/// Formats a color as a CSS hex color for embedding in SVG/CSS.
///
/// Emits `#rrggbb` for fully opaque colors and `#rrggbbaa` when the input
/// has any transparency, so translucent theme colors (e.g. `ghost_element_hover`
/// from Zed's UI palette) round-trip without silently losing their alpha.
pub(crate) fn css_color(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let r = (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (rgba.a.clamp(0.0, 1.0) * 255.0).round() as u8;
    if a == 0xff {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

pub use postprocess::util::text_color_for_background;

/// Where a diagram's nodes and edge labels ended up in the rendered SVG.
///
/// All rectangles are `[x, y, width, height]` in the SVG's `viewBox`
/// coordinate system, with every enclosing `transform` already folded in, so a
/// consumer only has to map from displayed pixels into `viewBox` units to
/// hit-test the diagram. Flowcharts, ER diagrams and state diagrams are mapped;
/// every other diagram kind yields an empty geometry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiagramGeometry {
    /// The SVG's `viewBox`: `[min-x, min-y, width, height]`.
    pub view_box: [f32; 4],
    pub nodes: Vec<NodeGeometry>,
    pub edges: Vec<EdgeGeometry>,
}

impl DiagramGeometry {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeGeometry {
    /// The node's id as written in the mermaid source (a flowchart node id, an
    /// ER entity name or a state id), with the prefix and the `-{n}` suffix
    /// that merman adds in the SVG stripped off.
    pub node_id: String,
    /// The label as rendered, with each line of a multi-line label on its own
    /// line. Empty when the node has no label of its own, and when the one it
    /// has could not be told apart from another element's.
    pub label: String,
    /// The node's shape, in `viewBox` coordinates.
    pub rect: [f32; 4],
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeGeometry {
    /// merman's own edge id: `L_{from}_{to}_{n}` for a flowchart,
    /// `id_{from}_{to}_{n}` for an ER relationship, `edge{n}` for a state
    /// transition.
    pub edge_id: String,
    /// The id of the node the edge leaves, empty for the kinds whose SVG does
    /// not name it (state diagrams, which number their transitions instead).
    pub from: String,
    /// The id of the node the edge enters, empty under the same condition as
    /// [`Self::from`].
    pub to: String,
    /// Which edge this is among those sharing the same `from` and `to`, in
    /// source order.
    pub occurrence: usize,
    /// Which connection this is among all of the diagram's, counting the
    /// statements that wrote them in document order, for the kinds that
    /// identify an edge by position rather than by its ends. `None` whenever
    /// `from` and `to` are the identity instead.
    pub statement_index: Option<usize>,
    /// The label as rendered, or `None` for an unlabelled edge.
    pub label: Option<String>,
    /// The label's background box, in `viewBox` coordinates.
    pub label_rect: Option<[f32; 4]>,
}

/// See the [module-level docs][crate] for more info.
#[ztracing::instrument(skip_all)]
pub fn render_to_svg(source: &str, theme: &MermaidTheme) -> Result<String> {
    render_to_svg_with_geometry(source, theme).map(|(svg, _)| svg)
}

/// Renders like [`render_to_svg`], additionally reporting where the diagram's
/// nodes and edge labels ended up, so a caller can map a click on the raster
/// back to the mermaid source.
#[ztracing::instrument(skip_all)]
pub fn render_to_svg_with_geometry(
    source: &str,
    theme: &MermaidTheme,
) -> Result<(String, DiagramGeometry)> {
    let svg = render::render_mermaid(source, theme)?;
    postprocess::postprocess(&svg, theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_diagram_with_mixed_weight_combining_marks_does_not_panic() {
        const IBM_PLEX_REGULAR: &[u8] =
            include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
        const IBM_PLEX_SEMIBOLD: &[u8] =
            include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf");

        let zalgo = "Ne\u{0301}\u{0302}\u{0303}\u{0304}\u{0306}\u{0307}\u{0308}\u{030a}d";
        let source = format!("flowchart TD\n  A[\"**{zalgo}** {zalgo}\"]");
        let svg = render_to_svg(&source, &MermaidTheme::default())
            .expect("mermaid diagram should render to SVG");

        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(IBM_PLEX_REGULAR.to_vec());
        db.load_font_data(IBM_PLEX_SEMIBOLD.to_vec());
        db.set_sans_serif_family("IBM Plex Sans");
        let options = usvg::Options {
            fontdb: std::sync::Arc::new(db),
            ..Default::default()
        };

        usvg::Tree::from_data(svg.as_bytes(), &options)
            .expect("rasterizing mermaid text should not panic");
    }

    /// An ER diagram whose attribute-block tokens begin with a multibyte
    /// UTF-8 character (e.g. CJK type/field names) must not panic while the
    /// lexer probes for the two-character `PK`/`FK`/`UK` keys.
    #[test]
    fn er_multibyte_attribute_does_not_crash() {
        let source = "erDiagram\n顧客 {\n  文字列 名前\n}";
        let _ = render_to_svg(source, &MermaidTheme::default());
    }

    /// A flowchart with mutually nested subgraphs (`A` contains `B` and `B`
    /// contains `A`) is an invalid containment cycle. Rendering it must return
    /// gracefully rather than overflowing the stack and aborting the process.
    #[test]
    fn cyclic_subgraphs_do_not_crash() {
        let source = "flowchart TD\n  subgraph A\n    B\n  end\n  subgraph B\n    A\n  end";
        let result = render_to_svg(source, &MermaidTheme::default());
        if let Err(err) = result {
            let message = format!("{err:#}");
            assert!(
                message.contains("cycle"),
                "expected a cycle-related error, got: {message}"
            );
        }
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    fn geometry_of(source: &str) -> DiagramGeometry {
        let (_, geometry) = render_to_svg_with_geometry(source, &MermaidTheme::default())
            .expect("mermaid diagram should render to SVG");
        geometry
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

    fn node<'a>(geometry: &'a DiagramGeometry, node_id: &str) -> &'a NodeGeometry {
        geometry
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .unwrap_or_else(|| panic!("node {node_id} should be mapped in {:?}", geometry.nodes))
    }

    /// The node boxes must be absolute `viewBox` coordinates: in a
    /// left-to-right flowchart each node sits strictly to the right of its
    /// predecessor, which only holds if every enclosing `transform` was folded
    /// in. Forgetting the outer translate would pile every node onto the same
    /// local origin instead.
    #[test]
    fn node_boxes_are_absolute_view_box_coordinates() {
        let geometry = geometry_of("flowchart LR\n  A[one] --> B[two] --> C[three]");
        assert_eq!(
            geometry
                .nodes
                .iter()
                .map(|node| node.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
        let [a, b, c] = ["A", "B", "C"].map(|id| node(&geometry, id).rect);
        assert!(a[0] + a[2] <= b[0], "{a:?} should end before {b:?}");
        assert!(b[0] + b[2] <= c[0], "{b:?} should end before {c:?}");
        for rect in [a, b, c] {
            assert!(
                rect[2] > 0.0 && rect[3] > 0.0,
                "{rect:?} should have a size"
            );
            assert!(
                contains(geometry.view_box, rect),
                "{rect:?} should be inside {:?}",
                geometry.view_box
            );
        }

        let vertical = geometry_of("flowchart TD\n  A[one] --> B[two]");
        let top = node(&vertical, "A").rect;
        let bottom = node(&vertical, "B").rect;
        assert!(
            top[1] + top[3] <= bottom[1],
            "{top:?} should end above {bottom:?}"
        );
    }

    /// Labels, subgraphs and edge labels of a realistic Japanese diagram.
    #[test]
    fn geometry_maps_labels_subgraphs_and_edge_labels() {
        let geometry = geometry_of(
            "flowchart TD\n  A[開始] --> B{判定}\n  B -->|はい| C[処理]\n  B -- いいえ --> D((終了))\n  subgraph S[グループ]\n    C\n  end",
        );

        assert_eq!(node(&geometry, "A").label, "\u{958b}\u{59cb}");
        assert_eq!(node(&geometry, "B").label, "\u{5224}\u{5b9a}");
        assert_eq!(node(&geometry, "C").label, "\u{51e6}\u{7406}");
        assert_eq!(node(&geometry, "D").label, "\u{7d42}\u{4e86}");
        // The subgraph is a cluster, not a node, and must not be mapped.
        assert!(geometry.nodes.iter().all(|node| node.node_id != "S"));

        for (index, first) in geometry.nodes.iter().enumerate() {
            for second in &geometry.nodes[index + 1..] {
                assert!(
                    !overlaps(first.rect, second.rect),
                    "{} {:?} overlaps {} {:?}",
                    first.node_id,
                    first.rect,
                    second.node_id,
                    second.rect
                );
            }
        }

        let edges: Vec<_> = geometry
            .edges
            .iter()
            .map(|edge| (edge.from.as_str(), edge.to.as_str(), edge.label.as_deref()))
            .collect();
        assert_eq!(
            edges,
            vec![
                ("A", "B", None),
                ("B", "C", Some("\u{306f}\u{3044}")),
                ("B", "D", Some("\u{3044}\u{3044}\u{3048}")),
            ]
        );

        for edge in &geometry.edges {
            match (&edge.label, edge.label_rect) {
                (Some(_), Some(rect)) => {
                    assert!(
                        rect[2] > 0.0 && rect[3] > 0.0,
                        "{rect:?} should have a size"
                    );
                    assert!(
                        contains(geometry.view_box, rect),
                        "{rect:?} should be inside {:?}",
                        geometry.view_box
                    );
                    // `center_edge_labels` exists so labels do not end up under
                    // a node; a label box that does is a rendered diagram whose
                    // text the user cannot see, let alone click.
                    for node in &geometry.nodes {
                        assert!(
                            !overlaps(rect, node.rect),
                            "edge label {rect:?} overlaps node {} {:?}",
                            node.node_id,
                            node.rect
                        );
                    }
                }
                (None, None) => {}
                other => panic!("label and label_rect should agree, got {other:?}"),
            }
        }
    }

    /// Every flowchart node shape merman can draw has to yield a usable box,
    /// including the ones it emits as `<path>` rather than a plain rectangle.
    #[test]
    fn every_node_shape_yields_a_box() {
        let geometry = geometry_of(
            "flowchart LR\n  A[rect] --> B(round)\n  B --> C([stadium])\n  C --> D[[subroutine]]\n  D --> E[(cylinder)]\n  E --> F((circle))\n  F --> G>asym]\n  G --> H{rhombus}\n  H --> I{{hexagon}}\n  I --> J[/para/]\n  J --> K[\\para2\\]\n  K --> L(((double)))\n  L -->|lbl| M@{ shape: rect, label: \"meta\" }",
        );
        let expected = [
            ("A", "rect"),
            ("B", "round"),
            ("C", "stadium"),
            ("D", "subroutine"),
            ("E", "cylinder"),
            ("F", "circle"),
            ("G", "asym"),
            ("H", "rhombus"),
            ("I", "hexagon"),
            ("J", "para"),
            ("K", "para2"),
            ("L", "double"),
            ("M", "meta"),
        ];
        for (node_id, label) in expected {
            let node = node(&geometry, node_id);
            assert_eq!(node.label, label);
            assert!(
                node.rect[2] > 1.0 && node.rect[3] > 1.0,
                "{node_id} {:?} should have a size",
                node.rect
            );
            assert!(
                contains(geometry.view_box, node.rect),
                "{node_id} {:?} should be inside {:?}",
                node.rect,
                geometry.view_box
            );
        }
        assert_eq!(geometry.nodes.len(), expected.len());
    }

    /// The ids the geometry is keyed by have to survive the passes that run
    /// after it is collected, so that a merman release that renames or drops
    /// them turns this test red rather than silently breaking hit testing.
    #[test]
    fn collected_ids_survive_the_remaining_passes() {
        let (svg, geometry) = render_to_svg_with_geometry(
            "flowchart TD\n  A[one] -->|yes| B[two]",
            &MermaidTheme::default(),
        )
        .expect("mermaid diagram should render to SVG");

        assert!(!geometry.nodes.is_empty());
        for node in &geometry.nodes {
            assert!(
                svg.contains(&format!("-flowchart-{}-", node.node_id)),
                "node id {} should still be in the rendered SVG",
                node.node_id
            );
        }
        for edge in &geometry.edges {
            assert!(
                svg.contains(&format!("data-id=\"{}\"", edge.edge_id)),
                "edge id {} should still be in the rendered SVG",
                edge.edge_id
            );
        }
    }

    /// Edge ids embed the node ids, which may themselves contain underscores.
    #[test]
    fn edge_endpoints_survive_underscores_in_node_ids() {
        let geometry = geometry_of("flowchart TD\n  step_one[one] --> step_two[two]");
        let edge = geometry.edges.first().expect("one edge");
        assert_eq!(edge.from, "step_one");
        assert_eq!(edge.to, "step_two");
        assert_eq!(edge.occurrence, 0);
    }

    /// Parallel edges between the same pair are told apart by their position in
    /// document order, which is the order they were written in. merman's own id
    /// suffix is not that position: it counts every edge in the diagram, so the
    /// second `A --> B` of two comes out as `L_A_B_2`.
    #[test]
    fn parallel_edges_are_numbered() {
        let geometry = geometry_of("flowchart TD\n  A -->|first| B\n  A -->|second| B");
        let occurrences: Vec<_> = geometry
            .edges
            .iter()
            .map(|edge| (edge.occurrence, edge.label.as_deref()))
            .collect();
        assert_eq!(occurrences, vec![(0, Some("first")), (1, Some("second"))]);
    }

    /// An ER diagram names both ends of a relationship in the edge's id, using
    /// the entities' element ids, which carry an index of their own and may
    /// hold any character an entity name may — including the `_` the id is
    /// split on.
    #[test]
    fn er_entities_and_relationships_are_mapped() {
        let geometry = geometry_of(concat!(
            "erDiagram\n",
            "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : \"\u{6ce8}\u{6587}\u{3059}\u{308b}\"\n",
            "  \u{9867}\u{5ba2}[\u{304a}\u{5ba2}\u{69d8}] {\n",
            "    string \u{540d}\u{524d}\n",
            "  }\n",
        ));
        // The entity's box is the whole table, attributes included, and its
        // label is the alias rather than the name the source keys it by.
        let entity = node(&geometry, "\u{9867}\u{5ba2}");
        assert_eq!(entity.label, "\u{304a}\u{5ba2}\u{69d8}");
        assert!(
            contains(geometry.view_box, entity.rect),
            "{:?} should be inside {:?}",
            entity.rect,
            geometry.view_box
        );
        let plain = node(&geometry, "\u{6ce8}\u{6587}");
        assert_eq!(plain.label, "\u{6ce8}\u{6587}");
        assert!(!overlaps(entity.rect, plain.rect));

        let relationship = geometry.edges.first().expect("one relationship");
        assert_eq!(relationship.from, "\u{9867}\u{5ba2}");
        assert_eq!(relationship.to, "\u{6ce8}\u{6587}");
        assert_eq!(relationship.occurrence, 0);
        assert_eq!(relationship.statement_index, None);
        assert_eq!(
            relationship.label.as_deref(),
            Some("\u{6ce8}\u{6587}\u{3059}\u{308b}")
        );
        assert!(
            relationship
                .label_rect
                .is_some_and(|rect| rect[2] > 0.0 && rect[3] > 0.0)
        );

        let underscored = geometry_of("erDiagram\n  A_1 ||--o{ B : x\n  A_1 ||--o{ B : y\n");
        let pairs: Vec<_> = underscored
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.from.as_str(),
                    edge.to.as_str(),
                    edge.occurrence,
                    edge.label.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![("A_1", "B", 0, Some("x")), ("A_1", "B", 1, Some("y"))]
        );
    }

    /// A state diagram's SVG does not name a transition's ends at all: it
    /// numbers the transitions in the order the statements were written, and a
    /// composite state's body is written where the composite stands. Getting
    /// that order wrong would rewrite a different transition's label than the
    /// one that was double-clicked.
    #[test]
    fn state_states_and_transitions_are_mapped() {
        let geometry = geometry_of(concat!(
            "stateDiagram-v2\n",
            "  [*] --> \u{5f85}\u{6a5f}\n",
            "  \u{5f85}\u{6a5f} --> \u{5b9f}\u{884c} : \u{958b}\u{59cb}\n",
            "  state \u{8907}\u{5408} {\n",
            "    a --> b : go\n",
            "  }\n",
            "  \u{5b9f}\u{884c} --> [*]\n",
            "  note right of \u{5f85}\u{6a5f} : \u{6700}\u{521d}\u{306e}\u{72b6}\u{614b}\n",
        ));
        // A note's box, and the start and end markers, are nodes in the SVG
        // too, but no statement declares them as states, so they are not
        // offered for editing.
        let mut ids: Vec<&str> = geometry
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "b", "\u{5b9f}\u{884c}", "\u{5f85}\u{6a5f}"]);
        assert_eq!(
            node(&geometry, "\u{5f85}\u{6a5f}").label,
            "\u{5f85}\u{6a5f}"
        );
        for node in &geometry.nodes {
            assert!(
                contains(geometry.view_box, node.rect),
                "{} {:?} should be inside {:?}",
                node.node_id,
                node.rect,
                geometry.view_box
            );
        }

        let mut transitions: Vec<_> = geometry
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.statement_index.expect("a numbered transition"),
                    edge.label.as_deref(),
                )
            })
            .collect();
        transitions.sort_by_key(|(index, _)| *index);
        assert_eq!(
            transitions,
            vec![
                (0, None),
                (1, Some("\u{958b}\u{59cb}")),
                (2, Some("go")),
                (3, None)
            ],
            "the composite's transition is the third statement, not the last"
        );
        // Every labelled transition has a box to hit-test, even though merman
        // draws a state diagram's edge labels outside the group that names them.
        for edge in &geometry.edges {
            assert_eq!(
                edge.label.is_some(),
                edge.label_rect.is_some(),
                "{:?} has a label without a box, or the other way round",
                edge.edge_id
            );
            assert_eq!((&edge.from[..], &edge.to[..]), ("", ""));
        }
    }

    /// A described state is drawn with its description, while its id keeps
    /// keying it.
    #[test]
    fn a_state_description_is_the_label() {
        let geometry = geometry_of(concat!(
            "stateDiagram-v2\n",
            "  state \"\u{8aac}\u{660e}\" as s1\n",
            "  [*] --> s1\n",
            "  s1 --> s2 : go\n",
        ));
        assert_eq!(node(&geometry, "s1").label, "\u{8aac}\u{660e}");
        assert_eq!(node(&geometry, "s2").label, "s2");
    }

    /// Only the kinds whose ids and source syntax the editing UI can follow are
    /// mapped. The others still render, they just come back without geometry
    /// rather than with boxes that cannot be rewritten.
    #[test]
    fn other_diagram_kinds_have_no_geometry() {
        let mut rendered = 0;
        for source in [
            "sequenceDiagram\n  Alice->>Bob: Hi",
            "classDiagram\n  Animal <|-- Duck",
            "gantt\n  title A\n  section S\n  Task :a1, 2014-01-01, 30d",
            "pie title Pets\n  \"Dogs\" : 386",
            "mindmap\n  root\n    child",
        ] {
            // A diagram kind merman cannot render at all is not this test's
            // concern, but enough of them have to actually render for the
            // assertion below to mean anything.
            let Ok((_, geometry)) = render_to_svg_with_geometry(source, &MermaidTheme::default())
            else {
                continue;
            };
            rendered += 1;
            assert!(
                geometry.is_empty(),
                "{source} should have no geometry, got {geometry:?}"
            );
        }
        assert!(rendered >= 3, "only {rendered} of the diagrams rendered");
    }
}
