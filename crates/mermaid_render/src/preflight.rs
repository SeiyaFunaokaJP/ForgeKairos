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

/// merman draws one ring per radar tick; `ticks 999999999` never finishes.
const MAX_RADAR_TICKS: i64 = 1_000;

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

    let Ok(Some(metadata)) = engine.parse_metadata_sync(source, ParseOptions::default()) else {
        return Ok(());
    };
    match metadata.diagram_type.as_str() {
        // The endless loop is in merman's parser, so this check cannot use
        // the parsed model.
        "xychart" => check_xychart(&preprocessed.code),
        // merman's block parser expands `space:N` into `N` blocks, so that
        // count has to be checked before parsing too.
        "block" => {
            check_block_widths(&preprocessed.code)?;
            if before_layout {
                check_parsed_model(&engine, source)?;
            }
            Ok(())
        }
        "radar" | "c4" if before_layout => check_parsed_model(&engine, source),
        _ => Ok(()),
    }
}

fn check_parsed_model(engine: &Engine, source: &str) -> Result<()> {
    let Ok(Some(parsed)) =
        engine.parse_diagram_for_render_model_sync(source, ParseOptions::default())
    else {
        return Ok(());
    };
    check_model(&parsed.model)
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
        RenderSemanticModel::Radar(radar) => {
            let ticks = radar
                .options
                .ticks
                .as_i64()
                .or_else(|| radar.options.ticks.as_u64().map(|ticks| ticks as i64))
                .unwrap_or(0);
            if ticks > MAX_RADAR_TICKS {
                bail!("radar ticks {ticks} is too many to render (limit {MAX_RADAR_TICKS})");
            }
        }
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

#[derive(Clone, Copy)]
enum XAxis {
    Band,
    Linear { min: f64, max: f64 },
}

/// Replays how merman's xychart parser (`XyChartState` in
/// `merman-core/src/diagrams/xychart.rs`) derives x-axis categories for a
/// plot without categories. It steps from `min` by `(max - min) / (n - 1)`
/// until it passes `max`; when that step does not move the value (an
/// `x-axis 5 --> 5` range, or a range narrower than the float spacing) the
/// loop never ends and keeps allocating.
fn check_xychart(code: &str) -> Result<()> {
    let mut x_axis = XAxis::Band;
    let mut x_axis_set = false;
    let cleaned = xychart_strip_comments(code);
    let statements = xychart_split_statements(&cleaned);
    for statement in statements
        .iter()
        .map(|statement| statement.trim())
        .filter(|statement| !statement.is_empty())
        .skip(1)
    {
        if ["title", "accTitle", "accDescr", "y-axis"]
            .iter()
            .any(|keyword| strip_keyword(statement, keyword).is_some())
        {
            continue;
        }
        if let Some(rest) = strip_keyword(statement, "x-axis") {
            if let Some(axis) = xychart_x_axis(rest) {
                x_axis = axis;
                x_axis_set = true;
            }
            continue;
        }
        let Some(rest) =
            strip_keyword(statement, "line").or_else(|| strip_keyword(statement, "bar"))
        else {
            continue;
        };
        let Some(count) = xychart_plot_value_count(rest) else {
            continue;
        };
        if !x_axis_set {
            let (previous_min, previous_max) = match x_axis {
                XAxis::Linear { min, max } => (min, max),
                XAxis::Band => (f64::INFINITY, f64::NEG_INFINITY),
            };
            x_axis = XAxis::Linear {
                min: previous_min.min(1.0),
                max: previous_max.max(count as f64),
            };
            x_axis_set = true;
        }
        if let XAxis::Linear { min, max } = x_axis
            && !x_axis_steps_finish(min, max, count)
        {
            bail!(
                "xychart x-axis range {min} --> {max} cannot be divided into {count} points; \
                 widen the range or add categories"
            );
        }
    }
    Ok(())
}

fn x_axis_steps_finish(min: f64, max: f64, count: usize) -> bool {
    let denominator = count as f64 - 1.0;
    let step = (max - min) / denominator;
    let mut value = min;
    let mut steps = 0usize;
    while value <= max {
        steps += 1;
        // merman produces one category per value, plus at most one more
        // from rounding; anything beyond that means the step is not
        // advancing.
        if steps > count.saturating_add(1) {
            return false;
        }
        value += step;
        if denominator == 0.0 {
            break;
        }
    }
    true
}

fn strip_keyword<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    let statement = statement.trim_start();
    let head = statement.get(..keyword.len())?;
    head.eq_ignore_ascii_case(keyword)
        .then(|| &statement[keyword.len()..])
}

fn xychart_x_axis(rest: &str) -> Option<XAxis> {
    let text = rest.trim();
    if text.starts_with('[') {
        return Some(XAxis::Band);
    }
    if let Some((min, max)) = xychart_range(text) {
        return Some(XAxis::Linear { min, max });
    }
    let tail = xychart_text_prefix_tail(text)?.trim_start();
    if tail.starts_with('[') {
        return Some(XAxis::Band);
    }
    let (min, max) = xychart_range(tail)?;
    Some(XAxis::Linear { min, max })
}

fn xychart_range(input: &str) -> Option<(f64, f64)> {
    let (first, tail) = take_number_token(input.trim_start())?;
    let tail = tail.trim_start().strip_prefix("-->")?;
    let (second, tail) = take_number_token(tail.trim_start())?;
    if !tail.trim().is_empty() {
        return None;
    }
    Some((parse_number(first)?, parse_number(second)?))
}

fn take_number_token(input: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, character) in input.char_indices() {
        let sign = index == 0 && (character == '+' || character == '-');
        if sign || character.is_ascii_digit() || character == '.' {
            end = index + character.len_utf8();
            continue;
        }
        break;
    }
    (end > 0).then(|| input.split_at(end))
}

fn parse_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty()
        || !text
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '+' | '-' | '.'))
    {
        return None;
    }
    text.parse().ok()
}

fn xychart_text_prefix_tail(input: &str) -> Option<&str> {
    let text = input.trim_start();
    if let Some(body) = text.strip_prefix("\"`") {
        let end = body.find("`\"")?;
        return body.get(end + 2..);
    }
    if let Some(body) = text.strip_prefix('"') {
        let end = body.find('"')?;
        return body.get(end + 1..);
    }
    let end = text
        .find(|character: char| character.is_whitespace() || character == '[')
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    text.get(end..)
}

fn xychart_plot_value_count(rest: &str) -> Option<usize> {
    let text = rest.trim();
    let data = if text.starts_with('[') {
        text
    } else {
        xychart_text_prefix_tail(text)?.trim_start()
    };
    let inner = data.strip_prefix('[')?;
    let inner = &inner[..inner.find(']')?];
    let mut count = 0;
    for part in inner.split(',') {
        parse_number(part)?;
        count += 1;
    }
    Some(count)
}

/// The comment rules of merman's xychart parser: `%%` outside quotes ends the
/// line, and lines that are only a comment or directive are dropped.
fn xychart_strip_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let mut in_quote = false;
        let mut cut = line.len();
        let mut characters = line.char_indices().peekable();
        while let Some((index, character)) = characters.next() {
            if in_quote {
                if character == '"' {
                    in_quote = false;
                }
                continue;
            }
            if character == '"' {
                in_quote = true;
                continue;
            }
            if character == '%' && characters.peek().is_some_and(|(_, next)| *next == '%') {
                cut = index;
                break;
            }
        }
        let kept = &line[..cut];
        if kept.trim_start().starts_with("%%") {
            continue;
        }
        output.push_str(kept);
    }
    output
}

/// The statement rules of merman's xychart parser: newlines and `;` end a
/// statement unless they are inside quotes, `[...]` or `{...}`.
fn xychart_split_statements(input: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut in_markdown = false;
    let mut bracket_depth = 0i64;
    let mut brace_depth = 0i64;
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if in_markdown {
            current.push(character);
            if character == '`' && characters.peek() == Some(&'"') {
                current.push('"');
                characters.next();
                in_markdown = false;
            }
            continue;
        }
        if in_quote {
            current.push(character);
            if character == '"' {
                in_quote = false;
            }
            continue;
        }
        if character == '"' && characters.peek() == Some(&'`') {
            current.push('"');
            current.push('`');
            characters.next();
            in_markdown = true;
            continue;
        }
        if character == '"' {
            current.push(character);
            in_quote = true;
            continue;
        }
        match character {
            '[' => bracket_depth += 1,
            ']' => bracket_depth -= 1,
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            _ => {}
        }
        if (character == '\n' || character == ';') && bracket_depth == 0 && brace_depth == 0 {
            statements.push(std::mem::take(&mut current));
            continue;
        }
        current.push(character);
    }
    if !current.is_empty() {
        statements.push(current);
    }
    statements
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

    #[test]
    fn a_merman_panic_becomes_an_error() {
        // Half-typed C4 boundaries: merman inserts the boundary type at
        // argument index 2 without checking the length.
        assert_rejected(
            "C4Context\nSystem_Boundary(b) {\n}",
            "merman panicked while rendering",
        );
        assert_rejected(
            "C4Container\nEnterprise_Boundary() {\n  Container(c1, \"C1\")\n}",
            "merman panicked while rendering",
        );
        // The render thread is reusable after a panic.
        assert_renders("C4Context\nSystem_Boundary(b, \"x\") {\n}");
    }

    #[test]
    fn c4_boundaries_merman_cannot_lay_out_are_rejected() {
        assert_rejected("C4Context\nBoundary() {\n}", "needs an alias");
        assert_rejected("C4Context\nBoundary(, \"x\") {\n}", "needs an alias");
        assert_rejected("C4Context\nBoundary(\"\", \"x\") {\n}", "needs an alias");
        assert_rejected("C4Deployment\nDeployment_Node() {\n}", "needs an alias");
        assert_rejected(
            "C4Context\nBoundary(b1, \"a\") {\n  Boundary(b1, \"b\") {\n  }\n}",
            "contains itself",
        );
        assert_renders("C4Context\nBoundary(b1, \"a\") {\n  Person(p, \"P\")\n}");
    }

    #[test]
    fn xychart_ranges_merman_cannot_divide_are_rejected() {
        assert_rejected("xychart-beta\nx-axis 5 --> 5\nbar [1, 2]", "x-axis range");
        assert_rejected(
            "xychart-beta\nx-axis \"時間\" 5 --> 5\nline [1, 2, 3]",
            "x-axis range",
        );
        // Without an x-axis, the first plot fixes the range at 1 --> 1.
        assert_rejected("xychart-beta\nbar [1]\nline [1, 2]", "x-axis range");
        // A range narrower than the float spacing at its ends.
        assert_rejected(
            "xychart-beta\nx-axis 100000000000000000 --> 100000000000000032\nbar [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]",
            "x-axis range",
        );
        // Statements split by `;` and plots spanning lines are read the way
        // merman reads them.
        assert_rejected("xychart-beta; x-axis 2 --> 2; bar [1,\n 2]", "x-axis range");

        assert_renders("xychart-beta\nx-axis 5 --> 5\nbar [1]");
        assert_renders("xychart-beta\nx-axis 0 --> 0.5\nbar [1, 2]");
        assert_renders("xychart-beta\nx-axis 1 --> 0\nbar [1, 2]");
        assert_renders("xychart-beta\nx-axis [a]\nbar [1, 2]");
        assert_renders("xychart-beta\nbar [1, 2]\nline [1, 2, 3]");
        assert_renders("xychart-beta\n%% x-axis 5 --> 5\nbar [1, 2]");
    }

    #[test]
    fn xychart_comments_end_the_line_like_merman() {
        // merman cuts a trailing `%%` comment together with the newline, so
        // the axis joins the next line; after a blank line it stands alone
        // and loops. Checked without rendering so a regression cannot hang.
        assert!(check_before_render("xychart-beta\nx-axis 5 --> 5 %% c\n\nbar [1, 2]").is_err());
        assert!(check_before_render("xychart-beta\nx-axis 5 --> 5 %% c\nbar [1, 2]").is_ok());
        assert!(check_before_render("xychart-beta\nx-axis \"%%\" 5 --> 5\nbar [1, 2]").is_err());
    }

    #[test]
    fn x_axis_steps() {
        assert!(x_axis_steps_finish(0.0, 10.0, 11));
        assert!(x_axis_steps_finish(0.0, 1.0, 3));
        assert!(x_axis_steps_finish(5.0, 5.0, 1));
        assert!(x_axis_steps_finish(1.0, 0.0, 2));
        assert!(!x_axis_steps_finish(5.0, 5.0, 2));
        assert!(!x_axis_steps_finish(1e17, 1e17 + 32.0, 10));
    }

    #[test]
    fn sizes_that_exhaust_memory_are_rejected() {
        assert_rejected(
            "radar-beta\naxis a, b, c\ncurve x{1, 2, 3}\nticks 999999999",
            "radar ticks",
        );
        assert_renders("radar-beta\naxis a, b, c\ncurve x{1, 2, 3}\nticks 1000");

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
        assert!(check_before_parse("xychart-beta\nx-axis 5 --> 5 %% c\n\nbar [1, 2]").is_err());
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
