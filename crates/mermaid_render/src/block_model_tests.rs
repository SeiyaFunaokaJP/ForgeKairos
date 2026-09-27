use super::*;
use crate::{MermaidTheme, render_to_svg};
use merman::{Engine, ParseOptions};
use serde_json::Value;

/// The source `markdown::InsertMermaidDiagram` writes for a block diagram. The
/// editor's tests check that it is still the action's template.
const TEMPLATE: &str = "block-beta\n    %% columns で列の数を決め、space は空きマス、--> で矢印\n    columns 3\n    input[\"入力\"] process[\"処理\"] output[\"出力\"]\n    space log[\"記録\"] space\n    input --> process\n    process --> output\n    process --> log";

fn parse(source: &str) -> BlockDocument {
    parse_block(source).unwrap_or_else(|error| panic!("{source}\nshould parse: {error}"))
}

/// Emitting must reach a fixed point after one pass, and the text is what is
/// compared, so a line that vanished from both documents still fails.
fn assert_stable(source: &str) -> String {
    let first = parse(source);
    let first_text = emit_block(&first);
    let second = parse(&first_text);
    let second_text = emit_block(&second);
    assert_eq!(
        first_text, second_text,
        "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
    );
    assert_eq!(
        second, first,
        "the document changed through a round trip of:\n{first_text}"
    );
    first_text
}

fn assert_renders(source: &str) {
    let svg = render_to_svg(source, &MermaidTheme::default())
        .unwrap_or_else(|error| panic!("the source should render: {error}\n{source}"));
    assert!(
        !svg.contains("Syntax error in text"),
        "the source rendered an error diagram:\n{source}"
    );
}

fn node(id: &str, shape: Option<BlockShape>, label: &str, width: Option<u32>) -> BlockItem {
    BlockItem::Node(BlockNode {
        id: id.to_string(),
        shape,
        label: label.to_string(),
        width,
    })
}

fn entry(depth: usize, joined: bool, item: BlockItem) -> BlockEntry {
    BlockEntry {
        depth,
        joined,
        item,
    }
}

fn edge(from: &str, to: &str, label: Option<&str>, link: &str) -> BlockStatement {
    BlockStatement::Edge(BlockEdge {
        from: from.to_string(),
        to: to.to_string(),
        label: label.map(str::to_string),
        link: link.to_string(),
    })
}

fn error_of(source: &str) -> String {
    match parse_block(source) {
        Ok(document) => panic!("`{source}` should be refused, got {document:?}"),
        Err(error) => format!("{error}"),
    }
}

/// What `merman` lays out, reduced to what the model decides: for each
/// container, its children in order with their kind, id, label, width and
/// columns. Spaces are spelled out one cell at a time, the way `merman` keeps
/// them.
fn merman_layout(source: &str) -> (Value, Vec<Value>) {
    let parsed = Engine::new()
        .parse_diagram_sync(source, ParseOptions::strict())
        .unwrap_or_else(|error| panic!("merman should read {source}: {error}"))
        .unwrap_or_else(|| panic!("merman should find a diagram in {source}"));
    let model = parsed.model;
    let blocks = model["blocks"].as_array().cloned().unwrap_or_default();
    let tree = Value::Array(blocks.iter().map(reduce_merman_block).collect());
    let root_columns = model["blocksFlat"]
        .as_array()
        .and_then(|flat| flat.iter().find(|block| block["id"] == "root"))
        .map(|root| root["columns"].clone())
        .unwrap_or(Value::Null);
    let edges = model["edges"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|edge| {
            serde_json::json!({
                "start": edge["start"],
                "end": edge["end"],
                "label": edge["label"],
                "arrow": edge["arrowTypeEnd"],
            })
        })
        .collect::<Vec<_>>();
    let mut styling = model["blocksFlat"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|block| block.get("classes").is_some() || block.get("styles").is_some())
        .map(|block| {
            serde_json::json!({
                "id": block["id"],
                "classes": block.get("classes").cloned().unwrap_or(Value::Array(Vec::new())),
                "styles": block
                    .get("styles")
                    .and_then(Value::as_array)
                    .map(|styles| {
                        styles
                            .iter()
                            .map(|style| Value::from(decode_entities(style.as_str().unwrap_or_default()).trim_end_matches(';').to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    styling.sort_by_key(|value| value["id"].to_string());
    (
        serde_json::json!({ "columns": root_columns, "children": tree, "styling": styling }),
        edges,
    )
}

/// Undoes the placeholders `merman` puts in for `#name;` before it parses,
/// which it only turns back when drawing.
fn decode_entities(text: &str) -> String {
    const OPEN: char = '\u{fb02}';
    const CLOSE: &str = "\u{b6}\u{df}";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest
            .get(start + OPEN.len_utf8()..)
            .unwrap_or_default()
            .trim_start_matches('\u{b0}');
        let Some(end) = after.find(CLOSE) else {
            out.push_str(after);
            return out;
        };
        out.push('#');
        out.push_str(after.get(..end).unwrap_or_default());
        out.push(';');
        rest = after.get(end + CLOSE.len()..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

fn reduce_merman_block(block: &Value) -> Value {
    let kind = block["type"].as_str().unwrap_or_default();
    let generated = matches!(kind, "space")
        || (kind == "composite" && block["id"].as_str().is_some_and(|id| id.starts_with("id-")));
    serde_json::json!({
        "type": kind,
        "id": if generated { Value::Null } else { block["id"].clone() },
        "label": if kind == "space" { Value::Null } else { block["label"].clone() },
        "width": if kind == "space" { Value::Null } else { block.get("widthInColumns").cloned().unwrap_or(Value::Null) },
        "columns": block.get("columns").cloned().unwrap_or(Value::Null),
        "directions": block.get("directions").cloned().unwrap_or(Value::Null),
        "children": block["children"]
            .as_array()
            .map(|children| children.iter().map(reduce_merman_block).collect::<Vec<_>>())
            .unwrap_or_default(),
    })
}

fn shape_type(shape: Option<BlockShape>) -> &'static str {
    match shape {
        None => "na",
        Some(BlockShape::Square) => "square",
        Some(BlockShape::Round) => "round",
        Some(BlockShape::Stadium) => "stadium",
        Some(BlockShape::Subroutine) => "subroutine",
        Some(BlockShape::Cylinder) => "cylinder",
        Some(BlockShape::Circle) => "circle",
        Some(BlockShape::DoubleCircle) => "doublecircle",
        Some(BlockShape::Asymmetric) => "rect_left_inv_arrow",
        Some(BlockShape::Diamond) => "diamond",
        Some(BlockShape::Hexagon) => "hexagon",
        Some(BlockShape::LeanRight) => "lean_right",
        Some(BlockShape::LeanLeft) => "lean_left",
        Some(BlockShape::Trapezoid) => "trapezoid",
        Some(BlockShape::InvertedTrapezoid) => "inv_trapezoid",
    }
}

/// A label as `merman` reports it: without the Markdown backticks.
fn drawn_label(label: &str) -> String {
    if label.len() >= 2 && label.starts_with('`') && label.ends_with('`') {
        label
            .get(1..label.len() - 1)
            .unwrap_or_default()
            .to_string()
    } else {
        label.to_string()
    }
}

fn columns_value(columns: Option<BlockColumns>) -> Value {
    match columns {
        None => Value::Null,
        Some(BlockColumns::Auto) => Value::from(-1),
        Some(BlockColumns::Count(count)) => Value::from(count),
    }
}

/// The same reduction of the model, which has to match [`merman_layout`] for
/// every diagram the model opens.
fn model_layout(document: &BlockDocument) -> (Value, Vec<Value>) {
    fn children(entries: &[BlockEntry], start: usize, depth: usize) -> (Vec<Value>, usize) {
        let mut out = Vec::new();
        let mut index = start;
        while let Some(entry) = entries.get(index) {
            if entry.depth < depth {
                break;
            }
            index += 1;
            match &entry.item {
                BlockItem::Comment(_) => {}
                BlockItem::Space { width } => {
                    for _ in 0..width.unwrap_or(1) {
                        out.push(serde_json::json!({
                            "type": "space", "id": Value::Null, "label": Value::Null,
                            "width": Value::Null, "columns": Value::Null,
                            "directions": Value::Null, "children": [],
                        }));
                    }
                }
                BlockItem::Node(node) => out.push(serde_json::json!({
                    "type": shape_type(node.shape),
                    "id": node.id,
                    "label": if node.shape.is_some() { drawn_label(&node.label) } else { node.id.clone() },
                    "width": node.width.unwrap_or(1),
                    "columns": Value::Null,
                    "directions": Value::Null,
                    "children": [],
                })),
                BlockItem::Arrow(arrow) => out.push(serde_json::json!({
                    "type": "block_arrow",
                    "id": arrow.id,
                    "label": arrow.label,
                    "width": arrow.width.unwrap_or(1),
                    "columns": Value::Null,
                    "directions": arrow.directions.iter().map(|direction| direction.keyword()).collect::<Vec<_>>(),
                    "children": [],
                })),
                BlockItem::Group(group) => {
                    let (inner, next) = children(entries, index, depth + 1);
                    index = next;
                    out.push(serde_json::json!({
                        "type": "composite",
                        "id": group.id,
                        "label": group.label.as_deref().map(drawn_label).unwrap_or_default(),
                        "width": if group.id.is_some() { Value::from(group.width.unwrap_or(1)) } else { Value::Null },
                        "columns": columns_value(group.columns),
                        "directions": Value::Null,
                        "children": inner,
                    }));
                }
            }
        }
        (out, index)
    }

    let (tree, _) = children(&document.entries, 0, 0);
    let edges = document
        .statements
        .iter()
        .filter_map(|statement| match statement {
            BlockStatement::Edge(edge) => Some(edge),
            _ => None,
        })
        .map(|edge| {
            let head = edge.link.trim_matches('-');
            let arrow = match head {
                "x" => "arrow_cross",
                "o" => "arrow_circle",
                ">" => "arrow_point",
                _ => "",
            };
            serde_json::json!({
                "start": edge.from,
                "end": edge.to,
                "label": edge.label.clone().unwrap_or_default(),
                "arrow": arrow,
            })
        })
        .collect();

    let mut styled: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    for statement in &document.statements {
        let (ids, class, styles) = match statement {
            BlockStatement::Class {
                targets,
                class_name,
            } => (targets.clone(), Some(class_name.clone()), None),
            BlockStatement::Style { target, css } => (
                vec![target.clone()],
                None,
                Some(
                    css.split(',')
                        .map(|style| style.trim().trim_end_matches(';').to_string())
                        .filter(|style| !style.is_empty())
                        .collect::<Vec<_>>(),
                ),
            ),
            _ => continue,
        };
        for id in ids {
            let position = match styled.iter().position(|(styled_id, _, _)| *styled_id == id) {
                Some(position) => position,
                None => {
                    styled.push((id.clone(), Vec::new(), Vec::new()));
                    styled.len() - 1
                }
            };
            if let Some((_, classes, current_styles)) = styled.get_mut(position) {
                if let Some(class) = &class {
                    classes.push(class.clone());
                }
                if let Some(styles) = &styles {
                    *current_styles = styles.clone();
                }
            }
        }
    }
    let mut styling = styled
        .into_iter()
        .map(|(id, classes, styles)| serde_json::json!({ "id": id, "classes": classes, "styles": styles }))
        .collect::<Vec<_>>();
    styling.sort_by_key(|value| value["id"].to_string());

    (
        serde_json::json!({
            "columns": columns_value(document.columns).as_i64().map(Value::from).unwrap_or(Value::from(-1)),
            "children": tree,
            "styling": styling,
        }),
        edges,
    )
}

/// Whether the labels in `source` are ones `merman` reports as written, so
/// that the layouts can be compared label for label.
fn labels_are_plain(source: &str) -> bool {
    !source.contains(['&', '<', '#']) || !source.contains('"')
}

fn assert_same_layout(source: &str, document: &BlockDocument) {
    let (expected, expected_edges) = merman_layout(source);
    let (actual, actual_edges) = model_layout(document);
    if labels_are_plain(source) {
        assert_eq!(
            actual, expected,
            "the model reads {source} differently from merman"
        );
    } else {
        let strip = |value: &Value| {
            let mut text = value.to_string();
            text = text.replace(char::is_whitespace, "");
            text
        };
        assert_eq!(
            expected["children"].as_array().map(Vec::len),
            actual["children"].as_array().map(Vec::len),
            "the model reads {source} differently from merman:\n{}\n{}",
            strip(&expected),
            strip(&actual)
        );
    }
    assert_eq!(
        actual_edges, expected_edges,
        "the model reads the edges of {source} differently from merman"
    );
}

#[test]
fn the_insert_template_round_trips_byte_for_byte() {
    let document = parse(TEMPLATE);
    assert_eq!(document.columns, Some(BlockColumns::Count(3)));
    assert_eq!(
        document.entries,
        vec![
            entry(
                0,
                false,
                BlockItem::Comment(
                    "%% columns で列の数を決め、space は空きマス、--> で矢印".to_string()
                )
            ),
            entry(
                0,
                false,
                node("input", Some(BlockShape::Square), "入力", None)
            ),
            entry(
                0,
                true,
                node("process", Some(BlockShape::Square), "処理", None)
            ),
            entry(
                0,
                true,
                node("output", Some(BlockShape::Square), "出力", None)
            ),
            entry(0, false, BlockItem::Space { width: None }),
            entry(0, true, node("log", Some(BlockShape::Square), "記録", None)),
            entry(0, true, BlockItem::Space { width: None }),
        ]
    );
    assert_eq!(
        document.statements,
        vec![
            edge("input", "process", None, "-->"),
            edge("process", "output", None, "-->"),
            edge("process", "log", None, "-->"),
        ]
    );
    assert_eq!(emit_block(&document), TEMPLATE);
    assert_eq!(document.structure_error(), None);
    assert_same_layout(TEMPLATE, &document);
    assert_renders(TEMPLATE);
    // The drawing comparison the other tests rely on sees the blocks, and
    // tells a swap apart.
    assert_eq!(drawn_positions(TEMPLATE).0.len(), 4);
    assert_ne!(
        drawn_positions(
            "block-beta
  columns 2
  a b[\"B\"] c"
        ),
        drawn_positions(
            "block-beta
  columns 2
  a c b[\"B\"]"
        )
    );
}

/// Where `merman` puts every block it draws, by id, and where it puts the
/// blocks without one (anonymous groups, whose ids are random), in
/// drawing order. Rounded, so that the same layout compares equal.
fn drawn_positions(source: &str) -> (Vec<(String, [i64; 4])>, Vec<[i64; 4]>) {
    let layouted = merman::render::layout_diagram_sync(
        &Engine::new(),
        source,
        ParseOptions::strict(),
        &merman::render::headless_layout_options(),
    )
    .unwrap_or_else(|error| panic!("merman should lay out {source}: {error}"))
    .unwrap_or_else(|| panic!("merman should find a diagram in {source}"));
    let layout = serde_json::to_value(&layouted.layout).unwrap_or(Value::Null);
    let nodes = layout["BlockDiagram"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut named = Vec::new();
    let mut generated = Vec::new();
    for node in nodes {
        let place = ["x", "y", "width", "height"]
            .map(|key| (node[key].as_f64().unwrap_or(f64::NAN) * 10.).round() as i64);
        let id = node["id"].as_str().unwrap_or_default().to_string();
        if id.starts_with("id-") {
            generated.push(place);
        } else {
            named.push((id, place));
        }
    }
    named.sort();
    (named, generated)
}

/// The rewrites the emitter makes (blocks first named in an edge written where
/// they were named, statements moved below the blocks) must not move anything
/// in the drawing.
fn assert_drawn_alike(source: &str, emitted: &str) {
    assert_eq!(
        drawn_positions(source),
        drawn_positions(emitted),
        "merman places the blocks of\n{source}\ndifferently once written back as\n{emitted}"
    );
}

/// Every example `merman` ships is read the way `merman` reads it, written
/// back stably, drawn in the same places, and still drawn; the ones refused
/// are refused for a reason the model gives.
#[test]
fn every_merman_example_round_trips_or_is_refused_with_a_reason() {
    let Some(directory) = crate::merman_fixture_directory("block") else {
        return;
    };
    let mut paths = std::fs::read_dir(&directory)
        .expect("the fixture directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "mmd"))
        .collect::<Vec<_>>();
    paths.sort();
    assert!(paths.len() > 100, "found only {} examples", paths.len());
    let mut opened = 0;
    let mut refused: Vec<(String, String)> = Vec::new();
    for path in &paths {
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let source = std::fs::read_to_string(path).expect("a readable example");
        let document = match parse_block(&source) {
            Ok(document) => document,
            Err(error) => {
                refused.push((name, format!("{error}")));
                continue;
            }
        };
        opened += 1;
        assert_same_layout(&source, &document);
        assert_eq!(document.structure_error(), None, "{name}");
        let emitted = assert_stable(&source);
        let again = parse(&emitted);
        assert_same_layout(&emitted, &again);
        assert_same_layout(&source, &again);
        assert_drawn_alike(&source, &emitted);
        assert_renders(&emitted);
    }
    for (name, reason) in &refused {
        eprintln!("refused {name}: {reason}");
        assert!(
            reason.chars().any(|character| character > '\u{7f}'),
            "{name} should be refused with a reason in Japanese: {reason}"
        );
    }
    eprintln!("opened {opened} of {}", paths.len());
    assert!(
        opened * 10 >= paths.len() * 9,
        "only {opened} of {} examples open: {refused:#?}",
        paths.len()
    );
}

#[test]
fn nested_groups_widths_arrows_and_styles_survive() {
    let source = "block-beta\n  columns 3\n  a[\"A ラベル\"]:2 b\n  block:grp[\"グループ\"]:3\n    columns auto\n    %% 中のメモ\n    c((\"円\")) arrow<[\"進む\"]>(x, down) space:2\n    block\n      d\n    end\n  end\n  e --> f\n  a -- \"送る\" --x b\n  classDef hot fill:#f96,stroke:#333;\n  class a,b hot\n  style c fill:#bbf\n  %% 最後のメモ";
    let document = parse(source);
    assert_eq!(
        document.entries,
        vec![
            entry(
                0,
                false,
                node("a", Some(BlockShape::Square), "A ラベル", Some(2))
            ),
            entry(0, true, node("b", None, "", None)),
            entry(
                0,
                false,
                BlockItem::Group(BlockGroup {
                    id: Some("grp".to_string()),
                    label: Some("グループ".to_string()),
                    width: Some(3),
                    columns: Some(BlockColumns::Auto),
                })
            ),
            entry(1, false, BlockItem::Comment("%% 中のメモ".to_string())),
            entry(1, false, node("c", Some(BlockShape::Circle), "円", None)),
            entry(
                1,
                true,
                BlockItem::Arrow(BlockArrow {
                    id: "arrow".to_string(),
                    label: "進む".to_string(),
                    directions: vec![BlockArrowDirection::X, BlockArrowDirection::Down],
                    width: None,
                })
            ),
            entry(1, true, BlockItem::Space { width: Some(2) }),
            entry(
                1,
                false,
                BlockItem::Group(BlockGroup {
                    id: None,
                    label: None,
                    width: None,
                    columns: None,
                })
            ),
            entry(2, false, node("d", None, "", None)),
            entry(0, false, node("e", None, "", None)),
            entry(0, true, node("f", None, "", None)),
        ]
    );
    assert_eq!(
        document.statements,
        vec![
            edge("e", "f", None, "-->"),
            edge("a", "b", Some("送る"), "--x"),
            BlockStatement::ClassDef {
                name: "hot".to_string(),
                css: "fill:#f96,stroke:#333;".to_string()
            },
            BlockStatement::Class {
                targets: vec!["a".to_string(), "b".to_string()],
                class_name: "hot".to_string()
            },
            BlockStatement::Style {
                target: "c".to_string(),
                css: "fill:#bbf".to_string()
            },
            BlockStatement::Comment("%% 最後のメモ".to_string()),
        ]
    );
    assert_same_layout(source, &document);
    let emitted = assert_stable(source);
    assert_eq!(
        emitted,
        "block-beta\n    columns 3\n    a[\"A ラベル\"]:2 b\n    block:grp[\"グループ\"]:3\n        %% 中のメモ\n        columns auto\n        c((\"円\")) arrow<[\"進む\"]>(x, down) space:2\n        block\n            d\n        end\n    end\n    e f\n    e --> f\n    a -- \"送る\" --x b\n    classDef hot fill:#f96,stroke:#333;\n    class a,b hot\n    style c fill:#bbf\n    %% 最後のメモ"
    );
    assert_same_layout(&emitted, &parse(&emitted));
    assert_drawn_alike(source, &emitted);
    assert_renders(&emitted);
}

#[test]
fn the_preamble_keeps_its_place() {
    let source =
        "---\ntitle: 構成\n---\n%%{init: {\"theme\": \"dark\"}}%%\n%% 先頭のメモ\nblock\n  a";
    let document = parse(source);
    assert_eq!(document.keyword, BlockKeyword::Block);
    assert_eq!(
        document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (PreservedPosition::Preamble, "---\ntitle: 構成\n---"),
            (
                PreservedPosition::Preamble,
                "%%{init: {\"theme\": \"dark\"}}%%"
            ),
            (PreservedPosition::Preamble, "%% 先頭のメモ"),
        ]
    );
    let emitted = assert_stable(source);
    assert_eq!(
        emitted,
        "---\ntitle: 構成\n---\n%%{init: {\"theme\": \"dark\"}}%%\n%% 先頭のメモ\nblock\n    a"
    );
    assert_renders(&emitted);
}

/// The places `merman` puts a block depend on where it is first named, which
/// an edge can be; after parsing, every block is an entry and every edge names
/// placed blocks only, so the edges can go anywhere below them.
#[test]
fn an_edge_that_first_names_a_block_places_it_there() {
    let source = "block-beta\n  columns 2\n  a\n  b[\"B\"] --> c((\"C\"))\n  d\n  c --- a";
    let document = parse(source);
    assert_eq!(
        document.entries,
        vec![
            entry(0, false, node("a", None, "", None)),
            entry(0, false, node("b", Some(BlockShape::Square), "B", None)),
            entry(0, true, node("c", Some(BlockShape::Circle), "C", None)),
            entry(0, false, node("d", None, "", None)),
        ]
    );
    assert_same_layout(source, &document);
    let emitted = assert_stable(source);
    assert_eq!(
        emitted,
        "block-beta\n    columns 2\n    a\n    b[\"B\"] c((\"C\"))\n    d\n    b --> c\n    c --- a"
    );
    assert_same_layout(&emitted, &parse(&emitted));
    assert_drawn_alike(source, &emitted);
}

/// Mermaid merges a later mention that gives a placed block a shape into the
/// block; the model does the same, and keeps the edge.
#[test]
fn a_shape_given_in_a_later_edge_is_folded_into_the_block() {
    let source = "block-beta\n  id1 space id2\n  id1(\"Start\")-->id2(\"Stop\")\n  x[\"X\"] --> y\n  x[\"x\"] --> y";
    let document = parse(source);
    assert_eq!(
        document.entries.first().map(|entry| &entry.item),
        Some(&node("id1", Some(BlockShape::Round), "Start", None))
    );
    // `x["x"]` gives the label the id, which Mermaid does not take over a
    // label already written.
    assert!(
        document
            .entries
            .iter()
            .any(|entry| entry.item == node("x", Some(BlockShape::Square), "X", None))
    );
    assert_same_layout(source, &document);
    let emitted = assert_stable(source);
    assert_same_layout(&emitted, &parse(&emitted));
}

#[test]
fn constructs_that_would_be_changed_are_refused() {
    for (source, expected) in [
        ("block-beta\n  a\n  a", "2 回"),
        ("block-beta\n  a b a", "2 回"),
        (
            "block-beta\n  style a fill:#f00\n  a",
            "前に置かれていません",
        ),
        ("block-beta\n  class a hot\n  a", "前に置かれていません"),
        ("block-beta\n  a %% メモ", "`%%`"),
        ("block-beta\n  end", "`end`"),
        ("block-beta\n  block\n    a\n  end b", "`end` の後ろ"),
        ("block-beta\n  columns 2\n  columns 3", "2 回"),
        ("block-beta\n  a[/\"x\"]", "形として読まない"),
        ("block-beta\n  a space space b", "2 つ目の `space`"),
        ("block-beta\n  a space b:2", "幅を書けません"),
        (
            "block-beta\n  a[\"x\"]\n  a --> b\n  arrow<[\"x\"]>(right)\n  arrow[\"y\"] --> a",
            "形を辺の中で",
        ),
        (
            "block-beta\n  a\n  a[\"x\"] --> b\n  block:g\n  end\n  g[\"G\"] --> a",
            "形を辺の中で",
        ),
        ("block-beta columns 3", "`block-beta` の行"),
        ("block-beta\n  block:g a\n  end", "見出しの後ろ"),
        ("block-beta\n  a-b", "構造化編集に対応していません"),
        ("block-beta\n  a=b", "ID `a=b`"),
        ("block-beta\n  root", "ID `root`"),
        ("block-beta\n  a[x]", "`\"` で囲んで"),
        ("block-beta\n  a:0", "1 以上"),
        ("block-beta\n  %%{init: {\n  }}%%\n  a", "複数行"),
        ("block-beta\n  arrow<[\"x\"]>(sideways)", "向き"),
    ] {
        let error = error_of(source);
        assert!(
            error.contains(expected),
            "the reason for `{source}` should mention {expected}: {error}"
        );
    }
    // What `merman` itself cannot read is refused with its reason.
    assert!(parse_block("block-beta\n  a --> ").is_err());
}

#[test]
fn a_group_left_open_is_closed_and_a_block_line_with_blocks_is_split() {
    let source =
        "block\n  columns 3\n  B space\n  block BL\n    D\n  end\n  BL --> B\n  block\n    E";
    let document = parse(source);
    assert_same_layout(source, &document);
    let emitted = assert_stable(source);
    assert_eq!(
        emitted,
        "block\n    columns 3\n    B space\n    block\n        BL\n        D\n    end\n    block\n        E\n    end\n    BL --> B"
    );
    assert_same_layout(&emitted, &parse(&emitted));
    assert_drawn_alike(source, &emitted);
}

/// `a space b:2` is not something `merman` can read, so a width on the block
/// after an infix `space` starts a new line, and so does a second `space`.
#[test]
fn a_line_is_broken_where_merman_would_read_it_differently() {
    let mut document = parse("block-beta\n  a space b");
    if let Some(BlockItem::Node(node)) = document.entries.get_mut(2).map(|entry| &mut entry.item) {
        node.width = Some(2);
    }
    let emitted = emit_block(&document);
    assert_eq!(emitted, "block-beta\n    a space\n    b:2");
    assert_stable(&emitted);

    document
        .entries
        .insert(2, entry(0, true, BlockItem::Space { width: None }));
    let emitted = emit_block(&document);
    assert_eq!(emitted, "block-beta\n    a space\n    space b:2");
    let reparsed = parse(&emitted);
    assert_same_layout(&emitted, &reparsed);
    assert_renders(&emitted);
}

#[test]
fn renaming_an_id_follows_into_edges_classes_and_styles() {
    let mut document = parse(
        "block-beta\n  a b\n  block:g\n    c\n  end\n  a --> b\n  g --> a\n  classDef hot fill:#f00\n  class a,c hot\n  style a stroke:#000",
    );
    document.rename_ids(&[
        ("a".to_string(), "開始".to_string()),
        ("g".to_string(), "枠".to_string()),
    ]);
    assert_eq!(
        emit_block(&document),
        "block-beta\n    開始 b\n    block:枠\n        c\n    end\n    開始 --> b\n    枠 --> 開始\n    classDef hot fill:#f00\n    class 開始,c hot\n    style 開始 stroke:#000"
    );
    assert_eq!(document.structure_error(), None);
    assert_stable(&emit_block(&document));
}

#[test]
fn removing_an_entry_takes_what_names_it_along() {
    let source = "block-beta\n  a b\n  block:g\n    c\n    d\n  end\n  a --> b\n  c --> a\n  classDef hot fill:#f00\n  class a,c hot\n  class d hot\n  style c stroke:#000";
    let mut document = parse(source);
    let group = document
        .entries
        .iter()
        .position(|entry| entry.item.id() == Some("g"))
        .expect("the group is an entry");
    // The form removes the rows of the statements that went by these indices.
    assert_eq!(document.remove_entry(group), Some(vec![1, 4, 5]));
    assert_eq!(
        emit_block(&document),
        "block-beta\n    a b\n    a --> b\n    classDef hot fill:#f00\n    class a hot"
    );
    assert_eq!(document.structure_error(), None);
    assert_eq!(document.remove_entry(99), None);
}

/// Switching an item's kind keeps its id, label and width where the new kind
/// has them, and a group that stops being one lets its children out in place.
#[test]
fn converting_an_entry_keeps_what_the_kinds_share() {
    let mut document = parse("block-beta\n  a[\"あ\"]:2\n  block:g\n    b\n  end\n  space:3");
    assert!(document.convert_entry(0, BlockItemKind::Arrow));
    assert!(document.convert_entry(1, BlockItemKind::Node));
    assert!(document.convert_entry(3, BlockItemKind::Group));
    assert_eq!(
        emit_block(&document),
        "block-beta\n    a<[\"あ\"]>(right):2\n    g\n    b\n    block:group:3\n    end"
    );
    assert_eq!(document.structure_error(), None);
    assert!(document.convert_entry(0, BlockItemKind::Space));
    assert_eq!(
        document.entries.first().map(|entry| &entry.item),
        Some(&BlockItem::Space { width: Some(2) })
    );
    assert!(!document.convert_entry(9, BlockItemKind::Node));
}

#[test]
fn entries_move_indent_and_outdent_with_their_children() {
    let mut document = parse("block-beta\n  a\n  block:g\n    b\n  end\n  c");
    // c goes into the group above it.
    assert!(document.can_indent(3));
    assert!(document.indent_entry(3));
    assert_eq!(
        emit_block(&document),
        "block-beta\n    a\n    block:g\n        b\n        c\n    end"
    );
    // a has no group above it.
    assert!(!document.can_indent(0));
    assert!(!document.indent_entry(0));
    // The group, with both children, goes above a.
    assert_eq!(document.move_entry(1, false), Some(0));
    assert_eq!(
        emit_block(&document),
        "block-beta\n    block:g\n        b\n        c\n    end\n    a"
    );
    // b comes out of the group, right after it.
    assert_eq!(document.outdent_entry(1), Some(2));
    assert_eq!(
        emit_block(&document),
        "block-beta\n    block:g\n        c\n    end\n    b\n    a"
    );
    assert_eq!(document.outdent_entry(3), None);
    assert_eq!(document.structure_error(), None);
    assert_stable(&emit_block(&document));
}

#[test]
fn structure_errors_are_reported() {
    let base = parse("block-beta\n  a b\n  a --> b\n  class a hot\n  style b fill:#f00");
    assert_eq!(base.structure_error(), None);

    let cases: Vec<(Box<dyn Fn(&mut BlockDocument)>, &str)> = vec![
        (
            Box::new(|document| document.rename_ids(&[("b".to_string(), "a".to_string())])),
            "2 つ",
        ),
        (
            Box::new(|document| {
                document.entries.remove(1);
            }),
            "`b` というブロックがありません",
        ),
        (
            Box::new(|document| {
                if let Some(entry) = document.entries.get_mut(0) {
                    entry.item = node("a b", None, "", None);
                }
            }),
            "ID `a b`",
        ),
        (
            Box::new(|document| {
                if let Some(entry) = document.entries.get_mut(0) {
                    entry.item = node("a", Some(BlockShape::Square), "\"引用\"", None);
                }
            }),
            "ラベル",
        ),
        (
            Box::new(|document| {
                if let Some(entry) = document.entries.get_mut(0) {
                    entry.item = node("a", Some(BlockShape::Square), "`始まりだけ", None);
                }
            }),
            "ラベル",
        ),
        (
            Box::new(|document| {
                if let Some(entry) = document.entries.get_mut(1) {
                    entry.depth = 1;
                }
            }),
            "字下げ",
        ),
        (
            Box::new(|document| document.columns = Some(BlockColumns::Count(0))),
            "列数",
        ),
        (
            Box::new(|document| {
                document.entries.push(entry(
                    0,
                    false,
                    BlockItem::Arrow(BlockArrow {
                        id: "arrow".to_string(),
                        label: String::new(),
                        directions: Vec::new(),
                        width: None,
                    }),
                ))
            }),
            "向き",
        ),
        (
            Box::new(|document| {
                document.entries.push(entry(
                    0,
                    false,
                    BlockItem::Group(BlockGroup {
                        id: None,
                        label: Some("x".to_string()),
                        width: None,
                        columns: None,
                    }),
                ))
            }),
            "ID の無いグループ",
        ),
        (
            Box::new(|document| {
                document.statements.push(BlockStatement::Style {
                    target: "a".to_string(),
                    css: "fill:#f00\nstroke:#000".to_string(),
                })
            }),
            "改行",
        ),
        (
            Box::new(|document| {
                document.statements.push(edge("a", "b", None, "->"));
            }),
            "辺の線",
        ),
        (
            Box::new(|document| {
                document.statements.push(BlockStatement::Class {
                    targets: vec!["a".to_string()],
                    class_name: "二 語".to_string(),
                })
            }),
            "クラス名",
        ),
        (
            Box::new(|document| {
                document.statements.push(BlockStatement::Class {
                    targets: vec!["a".to_string(), "無い".to_string()],
                    class_name: "hot".to_string(),
                })
            }),
            "`無い` というブロックがありません",
        ),
        (
            Box::new(|document| {
                document.statements.push(BlockStatement::Style {
                    target: "無い".to_string(),
                    css: "fill:#f00".to_string(),
                })
            }),
            "`無い` というブロックがありません",
        ),
        (
            Box::new(|document| {
                document
                    .entries
                    .push(entry(0, false, BlockItem::Space { width: Some(0) }))
            }),
            "幅",
        ),
    ];
    for (change, expected) in cases {
        let mut document = base.clone();
        change(&mut document);
        let error = document
            .structure_error()
            .unwrap_or_else(|| panic!("{document:?} should be refused ({expected})"));
        assert!(error.contains(expected), "expected {expected}: {error}");
    }
}

/// Everything the form accepts has to come back as itself and be drawn.
#[test]
fn every_value_the_validators_accept_round_trips() {
    let mut entries = Vec::new();
    for (index, shape) in BlockShape::ALL.into_iter().enumerate() {
        entries.push(entry(
            0,
            index % 3 != 0,
            node(
                &format!("n{index}"),
                Some(shape),
                if index % 2 == 0 {
                    "ラベル (括弧) [角] {波}"
                } else {
                    "`**太字**`"
                },
                (index % 4 == 0).then_some(2),
            ),
        ));
    }
    entries.push(entry(
        0,
        false,
        BlockItem::Arrow(BlockArrow {
            id: "矢印".to_string(),
            label: " ".to_string(),
            directions: BlockArrowDirection::ALL.to_vec(),
            width: Some(2),
        }),
    ));
    entries.push(entry(0, true, BlockItem::Space { width: Some(3) }));
    entries.push(entry(0, true, node("x1.o", None, "", None)));
    entries.push(entry(
        0,
        false,
        BlockItem::Group(BlockGroup {
            id: Some("group_1".to_string()),
            label: Some(String::new()),
            width: Some(2),
            columns: Some(BlockColumns::Count(2)),
        }),
    ));
    entries.push(entry(1, false, BlockItem::Comment("%% メモ".to_string())));
    entries.push(entry(1, false, node("中", None, "", None)));
    let document = BlockDocument {
        keyword: BlockKeyword::BlockBeta,
        columns: Some(BlockColumns::Auto),
        entries,
        statements: vec![
            edge("n0", "n1", Some(""), "==>"),
            edge("n1", "中", Some("ラベル: 付き"), "-.->"),
            edge("中", "group_1", None, "--o"),
            BlockStatement::ClassDef {
                name: "強調".to_string(),
                css: String::new(),
            },
            BlockStatement::Class {
                targets: vec!["n0".to_string(), "矢印".to_string()],
                class_name: "強調".to_string(),
            },
            BlockStatement::Style {
                target: "group_1".to_string(),
                css: "fill:#eee,stroke:#333".to_string(),
            },
        ],
        preserved_lines: Vec::new(),
    };
    assert_eq!(document.structure_error(), None);
    let emitted = emit_block(&document);
    let reparsed = parse(&emitted);
    assert_eq!(reparsed, document, "{emitted}");
    assert_same_layout(&emitted, &reparsed);
    assert_renders(&emitted);
}

#[test]
fn values_are_validated() {
    for id in [
        "a", "入力", "id2.1", "x", "o", "x.o", "A_1", "block2", "endpoint", "spacer",
    ] {
        assert!(is_valid_block_id(id), "{id}");
    }
    for id in [
        "", "a b", "a-b", "a:b", "a,b", "a%b", "a=b", "x==", "~~~", "a;", "a#b", "a\"b", "end",
        "space", "columns", "class", "classDef", "style", "block", "root", "a[b", "a]", "a/b",
    ] {
        assert!(!is_valid_block_id(id), "{id}");
    }
    for label in ["", " ", "ラベル", "a`", "`md`", "&nbsp;", "括弧 ( ) [ ]"] {
        assert!(is_valid_block_label(label), "{label}");
    }
    for label in ["\"", "`", "`始まりだけ", "改行\nあり"] {
        assert!(!is_valid_block_label(label), "{label}");
    }
    for link in [
        "-->", "---", "--x", "--o", "---->", "==>", "===", "-.->", "-..-", ".-", "x-->", "<-->",
        "~~~",
    ] {
        assert!(is_valid_block_link(link), "{link}");
    }
    for link in ["", "->", "--", "==", "-", "~~", "- ->", "-.-x-", "-->>"] {
        assert!(!is_valid_block_link(link), "{link}");
    }
    assert_eq!(parse_block_width(""), Some(None));
    assert_eq!(parse_block_width(" 2 "), Some(Some(2)));
    assert_eq!(parse_block_width("0"), None);
    assert_eq!(parse_block_width("２"), None);
    assert_eq!(parse_block_width("-1"), None);
    assert_eq!(BlockColumns::from_text("auto"), Some(BlockColumns::Auto));
    assert_eq!(BlockColumns::from_text("3"), Some(BlockColumns::Count(3)));
    assert_eq!(BlockColumns::from_text("0"), None);
    assert_eq!(
        parse_arrow_directions("x, down"),
        Some(vec![BlockArrowDirection::X, BlockArrowDirection::Down])
    );
    assert_eq!(parse_arrow_directions(""), None);
    assert_eq!(parse_arrow_directions("right,"), None);
    assert!(is_valid_block_class_name("強調"));
    assert!(!is_valid_block_class_name("a b"));
    assert!(!is_valid_block_class_name(""));
    assert!(is_valid_block_css("fill:#f00, stroke:#333;"));
    assert!(!is_valid_block_css(" fill"));
    // The form names new items after these, so they have to be ids.
    for kind in BlockItemKind::ALL {
        assert!(is_valid_block_id(kind.id_prefix()), "{kind:?}");
    }
}

#[test]
fn an_empty_fence_opens_as_a_blank_block_diagram() {
    assert_eq!(parse(""), BlockDocument::empty());
    assert_eq!(parse("block-beta"), BlockDocument::empty());
    assert_eq!(emit_block(&BlockDocument::empty()), "block-beta");
    assert_eq!(BlockDocument::empty().structure_error(), None);
    assert_renders("block-beta");
}

#[test]
fn crlf_and_trailing_whitespace_are_tolerated() {
    let document = parse("block-beta\r\n  columns 2  \r\n  a[\"あ\"]  b  \r\n");
    assert_eq!(
        emit_block(&document),
        "block-beta\n    columns 2\n    a[\"あ\"] b"
    );
}

#[test]
fn unused_ids_do_not_collide() {
    let document = parse("block-beta\n  n n1");
    assert_eq!(document.unused_id("n"), "n2");
    assert_eq!(document.unused_id("新規"), "新規");
}

#[test]
fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
    for source in [
        "block-beta\n    a b",
        "block\n    a b",
        "block-beta\n    columns 3",
    ] {
        assert!(
            is_block_source(source),
            "{source} should be a block diagram"
        );
    }
    for source in [
        "blockquote\n  a",
        "blocks",
        "Block-beta",
        "block-betas",
        "",
        "%% block",
        "flowchart TD\n    A --> B",
    ] {
        assert!(
            !is_block_source(source),
            "{source} should not be a block diagram"
        );
    }
}

/// `merman` reads a link after a block across the line break, so a line that
/// opens with one continues the block above: `a` + `x-->b` is the edge
/// `a --> b`, not a block `x`. The model refuses it instead of writing back
/// three blocks.
#[test]
fn a_line_merman_reads_as_the_rest_of_an_edge_is_refused() {
    for source in [
        "block-beta\n  a\n  x-->b",
        "block-beta\n  a[\"A\"]\n  o--o b",
        "block-beta\n  a\n  %% メモ\n\n  x-- \"送る\" --> b",
        "block-beta\n  a c\n  x-.->b",
        "block-beta\n  block:g\n  x-->b\n  end",
        "block-beta\n  block:g[\"G\"]\n  o---b\n  end",
    ] {
        let error = error_of(source);
        assert!(
            error.contains("前の行のブロックから続く辺"),
            "the reason for `{source}` should say the line continues an edge: {error}"
        );
    }
    // Where `merman` does not look for a link, the same line is an edge of
    // its own.
    for source in [
        "block-beta\n  a:2\n  x-->b",
        "block-beta\n  a --> c\n  x-->b",
        "block-beta\n  a space\n  x-->b",
        "block-beta\n  a space c\n  x-->b",
        "block-beta\n  space\n  x-->b",
        "block-beta\n  columns 2\n  x-->b",
        "block-beta\n  block:g:2\n  x-->b\n  end",
        "block-beta\n  block\n  x-->b\n  end",
        "block-beta\n  a\n  xo-->b",
    ] {
        let document = parse(source);
        assert_same_layout(source, &document);
        let emitted = assert_stable(source);
        assert_drawn_alike(source, &emitted);
    }
}

/// Mermaid cuts a `%%{ … }%%` directive out of its line and reads the rest as
/// statements, so a directive with a block after it cannot become a comment
/// row, which would hide the block and move it below the others.
#[test]
fn a_directive_with_statements_after_it_is_refused() {
    let error = error_of("block-beta\n  columns 2\n  b --> c\n  %%{init: {}}%% a\n  d");
    assert!(error.contains("`a`"), "{error}");
    // A directive on its own line is kept where it was.
    let source = "block-beta\n  a\n  %%{init: {\"theme\": \"dark\"}}%%\n  b";
    let document = parse(source);
    assert_eq!(document.structure_error(), None);
    assert_eq!(
        assert_stable(source),
        "block-beta\n    a\n    %%{init: {\"theme\": \"dark\"}}%%\n    b"
    );

    assert!(is_valid_block_comment("%%{init: {}}%%"));
    assert!(is_valid_block_comment("%% メモ %%{init: {}}%% 続き"));
    for comment in ["%%{init: {}}%% a", "%%{ 閉じない", "%% メモ %%{ 閉じない"] {
        assert!(!is_valid_block_comment(comment), "{comment}");
    }
    for text in ["%%{", "前 %%{x}%% 後"] {
        assert!(!is_valid_block_label(text), "{text}");
        assert!(!is_valid_arrow_label(text), "{text}");
        assert!(!is_valid_edge_label(text), "{text}");
        assert!(!is_valid_block_css(text), "{text}");
        assert!(!is_valid_block_class_name(text), "{text}");
    }
    // With a directive in the preamble, a label opening another one would cut
    // the rest of the diagram away.
    let mut document = parse("%%{init: {}}%%\nblock-beta\n  a[\"A\"] b c");
    if let Some(BlockItem::Node(node)) = document.entries.get_mut(0).map(|entry| &mut entry.item) {
        node.label = "%%{".to_string();
    }
    assert!(document.structure_error().is_some());
}

/// Mermaid rewrites `="…"` inside anything shaped like an HTML tag before
/// reading the diagram, and such a "tag" can run across several labels,
/// taking their quotes: `a["<b"] c["k="] d["x>"]` is drawn as two blocks.
#[test]
fn labels_mermaid_would_rewrite_as_html_attributes_are_refused() {
    let source = "block-beta\n  a[\"<b\"] c[\"k=\"] d[\"x>\"]";
    assert!(error_of(source).contains("`'`"));

    let mut document = parse("block-beta\n  a[\"<b\"] c[\"k\"] d[\"x>\"]");
    assert_eq!(document.structure_error(), None);
    if let Some(BlockItem::Node(node)) = document.entries.get_mut(1).map(|entry| &mut entry.item) {
        node.label = "k=".to_string();
    }
    let error = document
        .structure_error()
        .expect("the rewritten labels should be refused");
    assert!(error.contains("`'`"), "{error}");

    // `<` that is not a tag, and `=` with no tag around it, are left alone.
    for source in [
        "block-beta\n  a[\"a < b\"] c[\"k=\"] d[\"x>\"]",
        "block-beta\n  a[\"k=\"] b[\"<br>\"]",
        "block-beta\n  arrow<[\"k=\"]>(right)",
    ] {
        let document = parse(source);
        assert_eq!(document.structure_error(), None, "{source}");
        assert_same_layout(source, &document);
    }
}
