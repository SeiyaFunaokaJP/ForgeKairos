//! Checks that run before a diagram reaches merman's layout.
//!
//! A panic inside merman is caught by the render thread, but some inputs make
//! merman loop forever or ask for tens of gigabytes at once, and an allocation
//! failure aborts the whole process no matter which thread it happens on. The
//! preview renders on every keystroke, so a half-typed line must never reach
//! those paths. Each check here mirrors a specific merman code path; the
//! comments name it so the check can be dropped when merman is fixed.

use anyhow::{Result, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};
use serde_json::Value;

/// Largest size accepted from a diagram's own config (front matter or
/// `%%{init}%%`) under one of [`SIZE_CONFIG_KEYS`]. merman's architecture
/// layout (manatee's FCoSE repulsion grid) allocates a cell per repulsion
/// range square of the drawing, so its memory grows with the square of
/// `architecture.padding` or `fontSize`: `10000000` requests about 160 GB.
const MAX_SIZE_CONFIG_NUMBER: f64 = 10_000.0;

/// Config keys that size the drawing, and that fuzzing found exhausting
/// memory with large values.
const SIZE_CONFIG_KEYS: &[&str] = &["padding", "fontSize", "iconSize", "width", "height"];

/// Largest number accepted under any other config key. Legitimate settings
/// go past 10000 (`gantt.useWidth: 12000`), and a grid growing with the square
/// of a value like the one above stays in the tens of megabytes at this size.
const MAX_CONFIG_NUMBER: f64 = 100_000.0;

/// Config keys that are limits or random seeds rather than sizes, so large
/// values are harmless.
const UNBOUNDED_CONFIG_KEYS: &[&str] = &[
    "maxTextSize",
    "maxEdges",
    "handDrawnSeed",
    "deterministicIDSeed",
    "seed",
];

/// merman lays block diagrams out on a grid of `columns` cells and expands
/// `space:N` into `N` cells; `columns 999999999` requests about 7 GB.
const MAX_BLOCK_COLUMNS: i64 = 1_000;

pub(crate) fn check_before_render(source: &str) -> Result<()> {
    check(source, true)
}

/// The part of [`check_before_render`] that guards merman's parser. The
/// structured editor parses without laying out, so a diagram that only the
/// layout cannot handle still opens there, to be fixed.
pub(crate) fn check_before_parse(source: &str) -> Result<()> {
    check(source, false)
}

fn check(source: &str, before_layout: bool) -> Result<()> {
    let engine = Engine::new();
    // Inputs merman cannot preprocess or detect are left to merman, which
    // reports them without doing any layout work.
    let Ok(preprocessed) = merman::preprocess_diagram(source, engine.registry()) else {
        return Ok(());
    };
    if before_layout {
        check_config_numbers(preprocessed.config.as_value(), "")?;
    }

    let Ok(metadata) = engine.parse_metadata_sync(source) else {
        return Ok(());
    };
    // merman 0.7 also looped forever on an xychart x-axis range it could not
    // step through and drew one ring per radar tick; 0.8 steps by the number
    // of values and clamps the ticks to 32 as Mermaid does, so neither needs
    // a check here any more.
    match metadata.diagram_type.as_str() {
        // merman's block parser expands `space:N` into `N` blocks, so that
        // count has to be checked before parsing too.
        "block" => {
            check_block_widths(preprocessed.code())?;
            if before_layout {
                check_parsed_model(&engine, source)?;
            }
            Ok(())
        }
        "c4" if before_layout => check_parsed_model(&engine, source),
        _ => Ok(()),
    }
}

fn check_parsed_model(engine: &Engine, source: &str) -> Result<()> {
    let Ok(Some(parsed)) =
        engine.parse_diagram_for_render_model_sync(source, ParseOptions::default())
    else {
        return Ok(());
    };
    check_model(parsed.model())
}

/// Finds the `space:N` widths merman's block parser expands into `N` blocks
/// while parsing. Other `:N` widths (`id:N`, `block:id:N`) are only stored
/// by the parser, and [`check_model`] checks them before layout; looking only
/// at `space` keeps a `:` in CSS (`style a stroke-width:5000px`) from counting.
/// Like merman, a quoted label may span lines, a `%%` comment ends at the
/// line break, and the number may follow the `:` after blanks and comments.
fn check_block_widths(code: &str) -> Result<()> {
    let mut in_quote = false;
    let mut in_comment = false;
    let mut previous: Option<char> = None;
    for (index, character) in code.char_indices() {
        let preceding = previous.replace(character);
        if in_comment {
            in_comment = character != '\n';
            continue;
        }
        if in_quote {
            in_quote = character != '"';
            continue;
        }
        match character {
            '"' => in_quote = true,
            '%' if code[index..].starts_with("%%") => in_comment = true,
            's' if preceding.is_none_or(|preceding| !is_block_id_character(preceding)) => {
                let Some(after_keyword) = code[index..].strip_prefix("space") else {
                    continue;
                };
                let Some(after_colon) = after_keyword
                    .trim_start_matches([' ', '\t'])
                    .strip_prefix(':')
                else {
                    continue;
                };
                let digits = block_width_digits(after_colon);
                if digits.is_empty() {
                    continue;
                }
                // Digits too long for i64 are over the limit as well.
                let width = digits.parse::<i64>().unwrap_or(i64::MAX);
                if width > MAX_BLOCK_COLUMNS {
                    bail!(
                        "block width `space:{digits}` is more than can be rendered (limit {MAX_BLOCK_COLUMNS})"
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Characters that continue a word before `space`, so that `white-space:` or
/// an id ending in `space` is not the keyword.
fn is_block_id_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-')
}

/// The digits merman's `parse_int` reads after a `:`, past blanks, line
/// breaks and `%%` comments.
fn block_width_digits(after_colon: &str) -> &str {
    let mut rest = after_colon;
    loop {
        rest = rest.trim_start();
        let Some(comment) = rest.strip_prefix("%%") else {
            break;
        };
        rest = comment
            .find('\n')
            .map_or("", |newline| &comment[newline + 1..]);
    }
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    &rest[..end]
}

fn check_config_numbers(value: &Value, key: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (child_key, child) in map {
                if UNBOUNDED_CONFIG_KEYS.contains(&child_key.as_str()) {
                    continue;
                }
                check_config_numbers(child, child_key)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                check_config_numbers(item, key)?;
            }
        }
        Value::Number(number) => {
            if let Some(number) = number.as_f64() {
                check_config_number(number, key)?;
            }
        }
        // merman reads sizes such as `fontSize` from strings too, with or
        // without a `px` suffix.
        Value::String(text) => {
            let text = text.trim();
            let digits = text
                .strip_suffix("px")
                .or_else(|| text.strip_suffix("PX"))
                .unwrap_or(text)
                .trim_end();
            if let Ok(number) = digits.parse::<f64>() {
                check_config_number(number, key)?;
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
    Ok(())
}

fn check_config_number(number: f64, key: &str) -> Result<()> {
    let limit = if SIZE_CONFIG_KEYS.contains(&key) {
        MAX_SIZE_CONFIG_NUMBER
    } else {
        MAX_CONFIG_NUMBER
    };
    if !number.is_finite() || number.abs() > limit {
        bail!("config value `{key}` is too large to render (limit {limit})");
    }
    Ok(())
}

fn check_model(model: &RenderSemanticModel) -> Result<()> {
    match model {
        RenderSemanticModel::Block(block) => {
            let mut pending = block.blocks_flat.iter().collect::<Vec<_>>();
            while let Some(node) = pending.pop() {
                if let Some(columns) = node.columns {
                    // `columns 0` divides by zero in merman's grid placement.
                    if columns == 0 || columns > MAX_BLOCK_COLUMNS {
                        bail!(
                            "block columns {columns} cannot be rendered (use 1 to {MAX_BLOCK_COLUMNS} or auto)"
                        );
                    }
                }
                for width in [node.width_in_columns, node.width].into_iter().flatten() {
                    if width > MAX_BLOCK_COLUMNS {
                        bail!(
                            "block `{}` spans {width} columns, more than can be rendered (limit {MAX_BLOCK_COLUMNS})",
                            node.id
                        );
                    }
                }
                pending.extend(node.children.iter());
            }
        }
        RenderSemanticModel::C4(c4) => {
            // merman's C4 layout never returns when a boundary has an empty
            // alias (`Boundary() {`, which is what a half-typed boundary
            // looks like) or when boundaries contain each other.
            for boundary in &c4.boundaries {
                if boundary.alias.trim().is_empty() {
                    bail!("C4 boundary needs an alias before it can be rendered");
                }
            }
            for boundary in &c4.boundaries {
                let mut parent = boundary.parent_boundary.as_str();
                let mut steps = 0;
                while !parent.is_empty() {
                    if parent == boundary.alias {
                        bail!("C4 boundary `{}` contains itself", boundary.alias);
                    }
                    steps += 1;
                    if steps > c4.boundaries.len() {
                        bail!("C4 boundaries contain each other in a cycle");
                    }
                    parent = c4
                        .boundaries
                        .iter()
                        .find(|candidate| candidate.alias == parent)
                        .map_or("", |candidate| candidate.parent_boundary.as_str());
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Renders on a separate thread so that a regression shows up as a
    /// failed test instead of a test run that never finishes.
    fn render_within_time_limit(source: &str) -> Result<String> {
        let (sender, receiver) = mpsc::channel();
        let source = source.to_string();
        std::thread::spawn(move || {
            sender
                .send(render_to_svg(&source, &MermaidTheme::default()))
                .ok();
        });
        receiver
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("rendering did not finish"))
    }

    fn assert_rejected(source: &str, expected: &str) {
        let error = render_within_time_limit(source)
            .expect_err(&format!("expected an error for:\n{source}"));
        let message = format!("{error:#}");
        assert!(
            message.contains(expected),
            "expected `{expected}` in `{message}` for:\n{source}"
        );
    }

    fn assert_renders(source: &str) {
        if let Err(error) = render_within_time_limit(source) {
            panic!("expected the diagram to render, got `{error:#}` for:\n{source}");
        }
    }

    /// merman 0.7 panicked on half-typed C4 boundaries (it inserted the
    /// boundary type at argument index 2 without checking the length); 0.8
    /// reads them, or refuses them with a parse error, without panicking.
    /// The panic path itself is covered by `tests::a_merman_panic_becomes_an_error`.
    #[test]
    fn half_typed_c4_boundaries_are_refused_or_drawn() {
        assert_rejected(
            "C4Context\nSystem_Boundary(b) {\n}",
            "C4 boundary must contain at least one diagram statement",
        );
        assert_rejected(
            "C4Container\nEnterprise_Boundary() {\n  Container(c1, \"C1\")\n}",
            "missing boundary alias",
        );
        assert_renders("C4Context\nSystem_Boundary(b) {\n  Person(p, \"P\")\n}");
        assert_renders("C4Context\nSystem_Boundary(b, \"x\") {\n  Person(p, \"P\")\n}");
    }

    #[test]
    fn c4_boundaries_merman_cannot_lay_out_are_rejected() {
        // merman 0.8 refuses an empty alias while parsing, before the layout
        // that never returned on one.
        for source in [
            "C4Context\nBoundary() {\n  Person(p, \"P\")\n}",
            "C4Deployment\nDeployment_Node() {\n  Person(p, \"P\")\n}",
        ] {
            assert_rejected(source, "missing boundary alias");
        }
        // An alias left out before the `,` or written as `""` gets past
        // merman's parser; the check here keeps it from the layout.
        for source in [
            "C4Context\nBoundary(, \"x\") {\n  Person(p, \"P\")\n}",
            "C4Context\nBoundary(\"\", \"x\") {\n  Person(p, \"P\")\n}",
        ] {
            assert_rejected(source, "needs an alias");
        }
        assert_rejected(
            "C4Context\nBoundary(b1, \"a\") {\n  Boundary(b1, \"b\") {\n    Person(p, \"P\")\n  }\n}",
            "contains itself",
        );
        assert_renders("C4Context\nBoundary(b1, \"a\") {\n  Person(p, \"P\")\n}");
    }

    /// merman 0.7 stepped through an xychart x-axis range that did not move
    /// (`5 --> 5`, or a range narrower than the float spacing) forever, and
    /// this module refused those ranges before merman saw them. 0.8 steps by
    /// the number of values, so they draw, as they do in Mermaid.
    #[test]
    fn xychart_ranges_merman_used_to_loop_on_are_drawn() {
        for source in [
            "xychart-beta\nx-axis 5 --> 5\nbar [1, 2]",
            "xychart-beta\nx-axis \"時間\" 5 --> 5\nline [1, 2, 3]",
            "xychart-beta\nbar [1]\nline [1, 2]",
            "xychart-beta\nx-axis 100000000000000000 --> 100000000000000032\nbar [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]",
            "xychart-beta; x-axis 2 --> 2; bar [1, 2]",
            "xychart-beta\nx-axis 5 --> 5 %% c\n\nbar [1, 2]",
            "xychart-beta\nx-axis 5 --> 5\nbar [1]",
            "xychart-beta\nx-axis 1 --> 0\nbar [1, 2]",
        ] {
            assert!(check_before_render(source).is_ok(), "{source}");
            assert!(check_before_parse(source).is_ok(), "{source}");
            assert_renders(source);
        }
    }

    #[test]
    fn sizes_that_exhaust_memory_are_rejected() {
        // merman 0.8 clamps the ticks to 32, as Mermaid does, where 0.7 drew
        // one ring per tick and never finished.
        let radar = "radar-beta\naxis a, b, c\ncurve x{1, 2, 3}\nticks 999999999";
        assert_renders(radar);
        let Ok(Some(parsed)) =
            Engine::new().parse_diagram_for_render_model_sync(radar, ParseOptions::default())
        else {
            panic!("merman should read {radar}");
        };
        let RenderSemanticModel::Radar(model) = parsed.model() else {
            panic!("{radar} should be a radar chart");
        };
        assert_eq!(model.options.ticks, serde_json::json!(32));

        assert_rejected("block-beta\ncolumns 999999999\na b", "block columns");
        assert_rejected("block-beta\ncolumns 0\na b", "block columns");
        assert_rejected("block-beta\na space:999999999 b", "block width");
        assert_rejected(
            "block-beta\na space:99999999999999999999999 b",
            "block width",
        );
        assert_rejected("block-beta\na:999999999 b", "spans 999999999 columns");
        assert_renders("block-beta\ncolumns auto\na space:3 b\nc:2 d[\"x:99999\"]");
        // merman reads the width after blanks, comments and line breaks.
        assert_rejected("block-beta\na space: 999999999 b", "block width");
        assert_rejected("block-beta\nspace :\n  999999999\na", "block width");
        assert_rejected("block-beta\nspace: %% wide\n999999999\na", "block width");
        assert!(check_before_render("block-beta\na\n%% space:999999999\nb").is_ok());
        assert!(check_before_render("block-beta\na[\"space:999999999\"]").is_ok());
        assert!(check_before_render("block-beta\na[\"x\nspace:999999999\"]").is_ok());
        // A `:` in CSS is not a width.
        assert_renders(
            "block-beta\na b\nstyle a stroke-width:5000px\nclassDef wide width:20000px\nclass b wide",
        );

        let architecture = "architecture-beta\n    group api(cloud)[API]\n    service db(database)[Database] in api";
        assert_rejected(
            &format!(
                "%%{{init: {{\"architecture\": {{\"padding\": 10000000}}}} }}%%\n{architecture}"
            ),
            "config value `padding`",
        );
        assert_rejected(
            &format!(
                "%%{{init: {{\"architecture\": {{\"fontSize\": \"10000000\"}}}} }}%%\n{architecture}"
            ),
            "config value `fontSize`",
        );
        assert_rejected(
            "---\nconfig:\n  xyChart:\n    height: 999999999\n---\nxychart-beta\nx-axis [a]\nbar [1]",
            "config value `height`",
        );
        assert_renders(&format!(
            "%%{{init: {{\"architecture\": {{\"padding\": 60}}, \"maxTextSize\": 90000000}} }}%%\n{architecture}"
        ));
        // Seeds only pick the random layout and the hand-drawn strokes.
        assert_renders(
            "%%{init: {\"handDrawnSeed\": 20260927, \"deterministicIDSeed\": \"20260927\", \"gitGraph\": {\"seed\": 99999}}}%%\nflowchart TD\nA-->B",
        );
        // Settings that do not size the drawing may pass 10000.
        assert_renders(
            "---\nconfig:\n  gantt:\n    useWidth: 12000\n---\ngantt\n    dateFormat YYYY-MM-DD\n    section A\n    Task :a1, 2026-01-01, 3d",
        );
        assert_renders(
            "%%{init: {\"sequence\": {\"width\": 2000, \"messageMargin\": 20000}}}%%\nsequenceDiagram\nA->>B: hi",
        );
        assert_rejected(
            "%%{init: {\"gantt\": {\"useWidth\": 1e9}}}%%\ngantt\n    section A\n    Task :a1, 2026-01-01, 3d",
            "config value `useWidth`",
        );
        assert_rejected(
            &format!("%%{{init: {{\"architecture\": {{\"padding\": 12000}}}} }}%%\n{architecture}"),
            "config value `padding`",
        );
    }

    #[test]
    fn the_editor_parse_is_guarded_only_against_parser_hazards() {
        assert!(check_before_parse("block-beta\na space:999999999 b").is_err());
        // Layout hazards are left for the preview to refuse, so the editor
        // can open the diagram and fix it.
        assert!(check_before_parse("block-beta\ncolumns 0\na b").is_ok());
        assert!(check_before_parse("C4Context\nBoundary(global, \"B\") {\n}").is_ok());
        assert!(
            check_before_parse(
                "%%{init: {\"architecture\": {\"padding\": 10000000}}}%%\narchitecture-beta\n    service db(database)[Database]"
            )
            .is_ok()
        );
    }
}
