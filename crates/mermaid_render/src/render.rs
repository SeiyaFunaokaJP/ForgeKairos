use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow};

use crate::{MermaidTheme, css_color};

pub(super) fn render_mermaid(source: &str, theme: &MermaidTheme) -> Result<String> {
    crate::preflight::check_before_render(source)?;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let diagram_id = format!("merman-{id}");

    let config = to_merman_config(theme);
    let renderer = merman::svg::HeadlessRenderer::new()
        .with_site_config(config)
        .with_vendored_text_measurer()
        .with_diagram_id(&diagram_id);
    // Apply merman's raster-safe pipeline before Zed-specific styling. The
    // pipeline handles generic rasterizer compatibility cleanup: foreignObject
    // fallback text, unsupported CSS removal, and invalid SVG attribute cleanup.
    // Zed also strips merman's existing `!important` declarations before
    // injecting its own theme CSS so host styling wins consistently in usvg/resvg.
    let pipeline = merman::svg::SvgPipeline::resvg_safe()
        .with_postprocessor(merman::svg::CssOverridePostprocessor::strip_existing_important());

    let svg = renderer
        .render_svg_with_pipeline_sync(source, &pipeline)
        .context("merman render failed")?
        .ok_or_else(|| anyhow!("merman returned no SVG for the given input"))?;

    Ok(svg)
}

fn to_merman_config(theme: &MermaidTheme) -> merman::MermaidConfig {
    let primary = css_color(theme.primary_color);
    let primary_text = css_color(theme.primary_text_color);
    let primary_border = css_color(theme.primary_border_color);
    let line = css_color(theme.line_color);
    let secondary = css_color(theme.secondary_color);
    let tertiary = css_color(theme.tertiary_color);
    let background = css_color(theme.background);
    let cluster_bg = css_color(theme.cluster_background);
    let cluster_border = css_color(theme.cluster_border);
    let edge_label_bg = css_color(theme.edge_label_background);
    let text = css_color(theme.text_color);
    let note_bg = css_color(theme.note_background);
    let note_border = css_color(theme.note_border);
    let actor_bg = css_color(theme.actor_background);
    let actor_border = css_color(theme.actor_border);
    let activation_bg = css_color(theme.activation_background);
    let activation_border = css_color(theme.activation_border);
    let er_odd = css_color(theme.er_attr_bg_odd);
    let er_even = css_color(theme.er_attr_bg_even);
    let git: [String; 8] = theme.git_branch_colors.map(css_color);
    let git_lbl: [String; 8] = theme.git_branch_label_colors.map(css_color);

    let mut theme_vars = serde_json::json!({
        "primaryColor": primary,
        "primaryTextColor": primary_text,
        "primaryBorderColor": primary_border,
        "lineColor": line,
        "secondaryColor": secondary,
        "secondaryTextColor": text,
        "tertiaryColor": tertiary,
        "tertiaryTextColor": text,
        "background": background,
        "mainBkg": primary,
        "nodeBorder": primary_border,
        "nodeTextColor": primary_text,
        "clusterBkg": cluster_bg,
        "clusterBorder": cluster_border,
        "titleColor": text,
        "edgeLabelBackground": edge_label_bg,
        "textColor": text,
        "fontFamily": theme.font_family,
        "noteBkgColor": note_bg,
        "noteBorderColor": note_border,
        "noteTextColor": text,
        "actorBkg": actor_bg,
        "actorBorder": actor_border,
        "actorTextColor": primary_text,
        "labelTextColor": text,
        "loopTextColor": text,
        "signalColor": text,
        "signalTextColor": text,
        "activationBkgColor": activation_bg,
        "activationBorderColor": activation_border,
        "classText": text,
        "labelColor": primary_text,
        "attributeBackgroundColorOdd": er_odd,
        "attributeBackgroundColorEven": er_even,
        "pieTitleTextColor": text,
        "pieSectionTextColor": text,
        "pieLegendTextColor": text,
        "pieStrokeColor": primary_border,
        "pieOuterStrokeColor": primary_border,
        "quadrant1Fill": primary,
        "quadrant2Fill": primary,
        "quadrant3Fill": primary,
        "quadrant4Fill": primary,
        "quadrant1TextFill": text,
        "quadrant2TextFill": text,
        "quadrant3TextFill": text,
        "quadrant4TextFill": text,
        "quadrantPointFill": line,
        "quadrantPointTextFill": text,
        "quadrantTitleFill": text,
        "quadrantXAxisTextFill": text,
        "quadrantYAxisTextFill": text,
        "quadrantExternalBorderStrokeFill": primary_border,
        "quadrantInternalBorderStrokeFill": primary_border,
    });

    if let Some(map) = theme_vars.as_object_mut() {
        for (((i, color), label), pie_number) in git.iter().enumerate().zip(&git_lbl).zip(1..) {
            map.insert(format!("cScale{i}"), color.clone().into());
            map.insert(format!("cScaleLabel{i}"), label.clone().into());
            map.insert(format!("pie{pie_number}"), color.clone().into());
            map.insert(format!("venn{pie_number}"), color.clone().into());
            // merman 0.8 strokes a treemap section in `cScalePeer{i}`, which
            // the base theme darkens from `cScale{i}` and which would then be
            // darker than a dark background.
            if theme.dark_mode {
                map.insert(format!("cScalePeer{i}"), color.clone().into());
            }
        }
        insert_newer_diagram_theme_variables(map, theme);
    }

    merman::MermaidConfig::from_value(serde_json::json!({
        "theme": "base",
        "darkMode": theme.dark_mode,
        "fontFamily": theme.font_family,
        // resvg can't rasterize HTML `<foreignObject>` labels, so merman's
        // raster-safe pipeline replaces them with wrapped native SVG text.
        "htmlLabels": true,
        "flowchart": {
            "htmlLabels": true,
            "padding": 16,
        },
        "themeVariables": theme_vars,
        // These diagrams read their colors from diagram-level config rather
        // than `themeVariables`, and default to black text on a light fill.
        "packet": {
            "startByteColor": text,
            "endByteColor": text,
            "labelColor": text,
            "titleColor": text,
            "blockStrokeColor": primary_border,
            "blockFillColor": primary,
        },
        "treemap": {
            "titleColor": text,
            "labelColor": text,
            "valueColor": text,
        },
        "radar": {
            "axisColor": line,
            "graticuleColor": primary_border,
        },
    }))
}

/// Colors for the diagram kinds whose `base` theme defaults are fixed light-mode
/// colors (black arrows, near-white lanes) instead of being derived from the
/// variables above, so they vanish on a dark background.
fn insert_newer_diagram_theme_variables(
    map: &mut serde_json::Map<String, serde_json::Value>,
    theme: &MermaidTheme,
) {
    let text = css_color(theme.text_color);
    let line = css_color(theme.line_color);
    let primary = css_color(theme.primary_color);
    let border = css_color(theme.primary_border_color);
    let edge_label_background = css_color(theme.edge_label_background);
    let cluster_background = css_color(theme.cluster_background);
    let cluster_border = css_color(theme.cluster_border);
    let accent = |index: usize| theme.git_branch_colors[index % theme.git_branch_colors.len()];
    let tinted = |index: usize| {
        css_color(crate::postprocess::blend_over_background(
            accent(index),
            theme.background,
            crate::postprocess::ACCENT_FILL_OPACITY,
        ))
    };

    let entries = [
        ("relationColor", line.clone()),
        ("relationLabelBackground", edge_label_background.clone()),
        ("relationLabelColor", text.clone()),
        ("requirementBackground", primary.clone()),
        ("requirementBorderColor", border.clone()),
        ("requirementTextColor", text.clone()),
        ("requirementEdgeLabelBackground", edge_label_background),
        ("archEdgeColor", line.clone()),
        ("archEdgeArrowColor", line.clone()),
        ("archGroupBorderColor", border.clone()),
        ("emUiFill", primary),
        ("emUiStroke", border),
        ("emCommandFill", tinted(0)),
        ("emCommandStroke", css_color(accent(0))),
        ("emReadModelFill", tinted(1)),
        ("emReadModelStroke", css_color(accent(1))),
        ("emEventFill", tinted(2)),
        ("emEventStroke", css_color(accent(2))),
        ("emProcessorFill", tinted(3)),
        ("emProcessorStroke", css_color(accent(3))),
        ("emRelationStroke", line.clone()),
        ("emArrowhead", line.clone()),
        ("emSwimlaneBackgroundOdd", cluster_background),
        ("emSwimlaneBackgroundStroke", cluster_border),
        ("vennTitleTextColor", text.clone()),
        ("vennSetTextColor", text.clone()),
    ];
    for (key, value) in entries {
        map.insert(key.to_string(), value.into());
    }
    map.insert(
        "treeView".to_string(),
        serde_json::json!({ "labelColor": text, "lineColor": line }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_stage_applies_resvg_safe_pipeline() {
        let html_label_source =
            "classDiagram\n    class Shelter {\n        -List~Animal~ animals\n    }";
        let html_label_svg =
            render_mermaid(html_label_source, &MermaidTheme::default()).expect("render failed");

        assert!(
            !html_label_svg.contains("<foreignObject"),
            "got: {html_label_svg}"
        );
        assert!(
            !html_label_svg.contains("&amp;lt;"),
            "got: {html_label_svg}"
        );

        let css_source = "sequenceDiagram\n    Alice->>Bob: Hello\n    Bob-->>Alice: Hi";
        let css_svg = render_mermaid(css_source, &MermaidTheme::default()).expect("render failed");

        assert!(!css_svg.contains("@keyframes"), "got: {css_svg}");
        assert!(!css_svg.contains("@-webkit-keyframes"), "got: {css_svg}");
        assert!(!css_svg.contains(":root"), "got: {css_svg}");
        assert!(!css_svg.contains("animation:"), "got: {css_svg}");
        assert!(!css_svg.contains("animation-name:"), "got: {css_svg}");
        assert!(!css_svg.contains("!important"), "got: {css_svg}");
    }

    #[test]
    fn long_flowchart_labels_render_as_wrapped_svg_text() {
        let source = "flowchart TD\n    \
            A[\"Pass 2: search transcript with annotation blocks excised, \
            map offsets back to buffer space\"] --> \
            |ambiguous or zero| B[\"Error describing where matches were found\"]";
        let svg = render_mermaid(source, &MermaidTheme::default()).expect("render failed");

        assert!(!svg.contains("<foreignObject"), "got: {svg}");
        assert!(!svg.contains(
            "Pass 2: search transcript with annotation blocks excised, map offsets back to buffer space</text>"
        ));
        let wrapped_line_count = svg.matches("merman-foreignobject-fallback-text").count();
        assert!(
            wrapped_line_count > 3,
            "expected long labels to wrap onto multiple SVG text lines, got {wrapped_line_count}: {svg}"
        );
    }

    /// The wrapped lines survive the fork's postprocessing: each keeps the
    /// theme's text color and size, and stays inside its node.
    #[test]
    fn wrapped_labels_keep_the_theme_after_postprocessing() {
        let source = "flowchart TD\n    \
            A[\"Pass 2: search transcript with annotation blocks excised, \
            map offsets back to buffer space\"]";
        let theme = MermaidTheme::default();
        let svg = crate::render_to_svg(source, &theme).expect("render failed");
        let text = css_color(theme.text_color);
        let lines = svg
            .split("<text ")
            .skip(1)
            .filter(|tag| tag.contains("merman-foreignobject-fallback-text"))
            .collect::<Vec<_>>();
        assert!(lines.len() > 3, "expected the label to wrap: {svg}");
        for line in &lines {
            assert!(line.contains(&format!("fill=\"{text}\"")), "{line}");
            assert!(line.contains("font-size:16px"), "{line}");
        }
        let (_, geometry) =
            crate::render_to_svg_with_geometry(source, &theme).expect("render failed");
        let node = geometry.nodes.first().expect("the node");
        let widest = lines
            .iter()
            .filter_map(|line| line.split('>').nth(1)?.split('<').next())
            .map(|content| content.chars().count())
            .max()
            .unwrap_or_default();
        // 16px text is well under 16px per character on average.
        assert!(
            (widest as f32) * 16.0 * 0.6 < node.rect[2],
            "{widest} {:?}",
            node.rect
        );
    }

    /// The drawn lines of every foreignObject fallback label: where each is
    /// anchored (its center) and how wide it is at most. A CJK character is
    /// taken as a full em and anything else as half of one, which is wider
    /// than the fonts draw it, so the check does not depend on the fonts
    /// installed.
    fn fallback_lines(svg: &str) -> Vec<(String, f32, f32, f32)> {
        svg.split("<text ")
            .skip(1)
            .filter(|tag| tag.contains("merman-foreignobject-fallback-text"))
            .filter_map(|tag| {
                let attribute = |name: &str| -> Option<f32> {
                    let tag = format!(" {tag}");
                    let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
                    tag.get(start..)?.split('"').next()?.parse().ok()
                };
                let font_size = tag
                    .split("font-size:")
                    .nth(1)
                    .and_then(|rest| rest.split("px").next())
                    .and_then(|size| size.trim().parse::<f32>().ok())
                    .unwrap_or(16.0);
                let content = tag.split_once('>')?.1.split('<').next()?.to_string();
                let width = content
                    .chars()
                    .map(|character| {
                        if character.is_ascii() {
                            font_size * 0.5
                        } else {
                            font_size
                        }
                    })
                    .sum();
                Some((content, attribute("x")?, attribute("y")?, width))
            })
            .collect()
    }

    const LONG_JAPANESE: &str = "長い日本語のラベルがノードの箱からはみ出さないかを確かめる";

    /// Mermaid wraps a long Japanese label inside its node: the browser breaks
    /// between CJK characters, and merman sizes the node the same way. Its
    /// foreignObject fallback text used to break only at spaces, so the label
    /// ran out of the node on one line; the vendored merman-render breaks it
    /// where the layout did (see `third_party/merman-render/FORK_CHANGES.md`).
    #[test]
    fn long_japanese_labels_wrap_inside_their_node() {
        let theme = MermaidTheme::default();
        for source in [
            format!("flowchart TD\n    B{{{LONG_JAPANESE}}}"),
            format!("flowchart TD\n    B[{LONG_JAPANESE}]"),
            format!("flowchart TD\n    B({LONG_JAPANESE})"),
            format!("flowchart TD\n    B(({LONG_JAPANESE}))"),
            format!("flowchart TD\n    B[\"日本語と English が混在した{LONG_JAPANESE}\"]"),
            format!("flowchart TD\n    B[\"一行目<br>{LONG_JAPANESE}\"]"),
            format!("flowchart TD\n    B[\"`**太字**の{LONG_JAPANESE}`\"]"),
            format!("flowchart LR\n    A -->|{LONG_JAPANESE}| B"),
            format!(
                "stateDiagram-v2\n    state \"{LONG_JAPANESE}\" as s1\n    s1 --> s2 : {LONG_JAPANESE}"
            ),
            format!("classDiagram\n    class A[\"{LONG_JAPANESE}\"]"),
            format!("erDiagram\n    顧客 ||--o{{ 注文 : \"{LONG_JAPANESE}\""),
        ] {
            let (svg, geometry) =
                crate::render_to_svg_with_geometry(&source, &theme).expect("render failed");
            let lines = fallback_lines(&svg);
            let drawn = lines
                .iter()
                .map(|(content, ..)| content.as_str())
                .collect::<String>();
            assert!(
                drawn.contains("はみ出さないかを確かめる"),
                "the label should be drawn as fallback text: {source}
{lines:?}"
            );
            let boxes = geometry
                .nodes
                .iter()
                .map(|node| node.rect)
                .chain(geometry.edges.iter().filter_map(|edge| edge.label_rect))
                .collect::<Vec<_>>();
            for (content, x, y, width) in &lines {
                let Some(rect) = boxes
                    .iter()
                    .filter(|rect| {
                        rect[0] <= *x
                            && *x <= rect[0] + rect[2]
                            && rect[1] <= *y
                            && *y <= rect[1] + rect[3]
                    })
                    .min_by(|a, b| (a[2] * a[3]).total_cmp(&(b[2] * b[3])))
                else {
                    continue;
                };
                assert!(
                    *width <= rect[2],
                    "{source}\nthe line {content:?} (about {width}px) is wider than its box {rect:?}"
                );
            }
            // Mermaid wraps neither a class name nor an ER relationship's
            // label; their box grows to the whole line instead.
            if !source.starts_with("classDiagram") && !source.starts_with("erDiagram") {
                let long_lines = lines
                    .iter()
                    .filter(|(content, ..)| content.chars().count() > 20)
                    .count();
                assert_eq!(long_lines, 0, "{source}: {lines:?}");
            }
        }

        // A mind map is not mapped for clicking, so its lines are held to the
        // 200px Mermaid wraps its labels at.
        let source = format!("mindmap\n  root(({LONG_JAPANESE}))\n    {LONG_JAPANESE}");
        let svg = crate::render_to_svg(&source, &theme).expect("render failed");
        let lines = fallback_lines(&svg);
        assert!(lines.len() > 2, "{lines:?}");
        for (content, _, _, width) in &lines {
            assert!(*width <= 200.0, "{content:?} is about {width}px wide");
        }
    }

    #[test]
    fn class_diagram_labels_keep_sixteen_pixel_font_size() {
        let source = r#"classDiagram
            class User {
                +String id
                +String name
                +signIn()
            }
        "#;
        let theme = MermaidTheme::default();
        let svg = render_mermaid(source, &theme).expect("render failed");
        let (svg, _geometry) =
            crate::postprocess::postprocess(&svg, &theme).expect("postprocess failed");
        let mut options = usvg::Options::default();
        options.fontdb_mut().load_system_fonts();
        let tree = usvg::Tree::from_str(&svg, &options).expect("SVG parsing failed");

        assert_eq!(text_font_size(tree.root(), "User"), Some(16.0));
        assert_eq!(text_font_size(tree.root(), "+String name"), Some(16.0));
    }

    #[test]
    fn mindmap_renders_drawable_svg() {
        let source = r#"mindmap
            root((Application))
                Frontend
                    Components
                    State
                    Routing
                Backend
                    API
                    Authentication
                    Jobs
                Data
                    Database
                    Cache
                    Backups
                Operations
                    Monitoring
                    Deployment
        "#;
        let theme = MermaidTheme::default();
        let svg = render_mermaid(source, &theme).expect("render failed");
        let (svg, _geometry) =
            crate::postprocess::postprocess(&svg, &theme).expect("postprocess failed");
        let mut options = usvg::Options::default();
        options.fontdb_mut().load_system_fonts();
        let tree = usvg::Tree::from_str(&svg, &options).expect("SVG parsing failed");

        assert!(tree.root().has_children(), "got: {svg}");
        assert!(
            text_font_size(tree.root(), "Application").is_some(),
            "got: {svg}"
        );
    }

    fn text_font_size(group: &usvg::Group, expected_text: &str) -> Option<f32> {
        for node in group.children() {
            match node {
                usvg::Node::Group(group) => {
                    if let Some(font_size) = text_font_size(group, expected_text) {
                        return Some(font_size);
                    }
                }
                usvg::Node::Text(text) => {
                    for chunk in text.chunks() {
                        if chunk.text() == expected_text {
                            return chunk.spans().first().map(|span| span.font_size().get());
                        }
                    }
                }
                usvg::Node::Path(_) | usvg::Node::Image(_) => {}
            }
        }

        None
    }
}
