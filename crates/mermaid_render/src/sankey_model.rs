//! A round-trippable editing model for Mermaid Sankey diagrams.
//!
//! A Sankey diagram is a header and CSV records of `source,target,value`, so
//! the model is a list of flows. The nodes are not declared anywhere: they are
//! the names the flows use, in the order they first appear, which is what
//! [`SankeyDocument::node_names`] returns.
//!
//! Three decisions are specific to Sankey diagrams:
//!
//! - `merman` trims every field and then reads `""` as `"` even outside
//!   quotes, and only a field whose very first character is `"` is a quoted
//!   one. [`parse_sankey`] reads a record the same way, and [`emit_sankey`]
//!   quotes a name only when it has to — and then writes the record without
//!   indentation, since an indented `"` is read as part of the name.
//! - A value keeps the spelling it was written with, as a pie slice's does.
//! - A document is opened only when writing it back gives `merman` the same
//!   flows the fence gave it, so a record this module reads differently from
//!   `merman` is refused instead of being quietly rewritten.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADERS: &[&str] = &["sankey-beta", "sankey"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SankeyFlow {
    pub source: String,
    pub target: String,
    /// The value as the source spells it, e.g. `30`, `0.597` or `1e3`.
    pub value: String,
}

/// An editable Sankey diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct SankeyDocument {
    /// `sankey-beta` or `sankey`, whichever the fence was written with.
    pub keyword: &'static str,
    pub flows: Vec<SankeyFlow>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl SankeyDocument {
    /// A blank Sankey diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: "sankey-beta",
            flows: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every node the flows name, in the order Mermaid creates them.
    pub fn node_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for flow in &self.flows {
            for name in [flow.source.as_str(), flow.target.as_str()] {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    /// Renames nodes in every flow that names them, all at once, so that
    /// swapping two names does not turn both into one.
    pub fn rename_nodes(&mut self, renames: &[(String, String)]) {
        let rename = |name: &mut String| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == name) {
                *name = new.clone();
            }
        };
        for flow in &mut self.flows {
            rename(&mut flow.source);
            rename(&mut flow.target);
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid draws, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        for flow in &self.flows {
            for name in [&flow.source, &flow.target] {
                if !is_valid_sankey_node(name) {
                    return Some(i18n::t!(
                        "The node name “{name}” cannot be used (it cannot be empty, have spaces at either end, or contain line breaks or `\"\"`)."
                    ));
                }
            }
            if !is_valid_sankey_value(&flow.value) {
                return Some(i18n::t!(
                    "The amount “{value}” of {source} → {target} cannot be used (only a number of 0 or more).",
                    value = flow.value,
                    source = flow.source,
                    target = flow.target
                ));
            }
        }
        if let Some(cycle) = self.cycle() {
            return Some(i18n::t!(
                "The flows form a cycle ({}). A Sankey diagram cannot draw a cycle.",
                cycle.join(" → ")
            ));
        }
        None
    }

    /// A chain of nodes that leads back to where it started, if there is one.
    /// `merman` lays the nodes out in columns by following the flows and gives
    /// up on a loop, a flow from a node to itself included.
    fn cycle(&self) -> Option<Vec<String>> {
        let names = self.node_names();
        let index_of = |name: &str| names.iter().position(|candidate| *candidate == name);
        let mut successors = vec![Vec::new(); names.len()];
        for flow in &self.flows {
            if let (Some(source), Some(target)) = (index_of(&flow.source), index_of(&flow.target))
                && let Some(list) = successors.get_mut(source)
            {
                list.push(target);
            }
        }

        #[derive(Clone, Copy, PartialEq)]
        enum Visit {
            Unseen,
            OnPath,
            Done,
        }
        let mut visits = vec![Visit::Unseen; names.len()];
        for start in 0..names.len() {
            if visits.get(start) != Some(&Visit::Unseen) {
                continue;
            }
            let mut path = vec![start];
            let mut cursors = vec![0usize];
            if let Some(visit) = visits.get_mut(start) {
                *visit = Visit::OnPath;
            }
            while let (Some(&node), Some(cursor)) = (path.last(), cursors.last_mut()) {
                let next = successors
                    .get(node)
                    .and_then(|list| list.get(*cursor))
                    .copied();
                *cursor += 1;
                let Some(next) = next else {
                    if let Some(visit) = visits.get_mut(node) {
                        *visit = Visit::Done;
                    }
                    path.pop();
                    cursors.pop();
                    continue;
                };
                match visits.get(next) {
                    Some(Visit::Unseen) => {
                        if let Some(visit) = visits.get_mut(next) {
                            *visit = Visit::OnPath;
                        }
                        path.push(next);
                        cursors.push(0);
                    }
                    Some(Visit::OnPath) => {
                        let from = path.iter().position(|step| *step == next).unwrap_or(0);
                        let cycle = path
                            .get(from..)
                            .unwrap_or_default()
                            .iter()
                            .chain([&next])
                            .filter_map(|step| names.get(*step).map(|name| name.to_string()))
                            .collect();
                        return Some(cycle);
                    }
                    _ => {}
                }
            }
        }
        None
    }
}

/// Whether `source` is a Sankey diagram: its first statement is `sankey-beta` or `sankey`.
pub fn is_sankey_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_sankey(source: &str) -> Result<SankeyDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = SankeyDocument {
        keyword: "sankey-beta",
        flows: Vec::new(),
        preserved_lines: preamble.preserved,
    };

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };
    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    document.keyword = match HEADERS.iter().find(|keyword| **keyword == header) {
        Some(keyword) => keyword,
        None => bail!(i18n::t!(
            "`{header}` cannot be read as the heading of a Sankey diagram (write only `sankey-beta` on its line)."
        )),
    };

    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        let start = index;
        index += 1;
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            let text = join_lines(lines.get(start..index).unwrap_or_default());
            refuse_statement_after_directive(&text)?;
            document
                .preserved_lines
                .push(PreservedLine { position, text });
            continue;
        }
        document.flows.push(parse_record(line)?);
        position = PreservedPosition::Bottom;
    }

    // `merman` refuses a Sankey diagram without a single record, but the editor
    // opens a bare header as a blank diagram for the user to fill in.
    if !document.flows.is_empty() {
        let original = merman_flows(source)?;
        // `merman` sanitizes a name before it looks the node up, so two names
        // the form tells apart can be one node in the drawing.
        if original.0 != document.node_names().len() {
            bail!(i18n::t!(
                "Some names are read by Mermaid as the same node (for example after it removes HTML tags). Edit it as text."
            ));
        }
        let emitted = emit_sankey(&document);
        let written = merman_flows(&emitted).map_err(|error| {
            anyhow!(i18n::t!(
                "Mermaid could not read the diagram written back ({error}). Edit it as text."
            ))
        })?;
        if original != written {
            bail!(i18n::t!(
                "Read by the structured editor, this diagram would have different flows than Mermaid reads. Edit it as text."
            ));
        }
    }
    if let Some(error) = document.structure_error() {
        bail!("{error}");
    }
    Ok(document)
}

type MermanFlows = (usize, Vec<(usize, usize, Option<f64>)>);

/// The flows `merman` reads from `source`, with each node replaced by the order
/// it was created in, so that the comparison does not depend on how `merman`
/// sanitizes a name for display.
fn merman_flows(source: &str) -> Result<MermanFlows> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(i18n::t!(
                "Mermaid could not parse this diagram ({error}). Edit it as text."
            ))
        })?
        .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
    let RenderSemanticModel::Sankey(model) = parsed.into_parts().1 else {
        bail!(i18n::t!("This diagram is not a Sankey diagram."));
    };
    let node_index = |id: &str| {
        model
            .graph
            .nodes
            .iter()
            .position(|node| node.id == id)
            .unwrap_or(usize::MAX)
    };
    let links = model
        .graph
        .links
        .iter()
        .map(|link| {
            (
                node_index(&link.source),
                node_index(&link.target),
                link.value.as_f64(),
            )
        })
        .collect();
    Ok((model.graph.nodes.len(), links))
}

/// Reads one CSV record the way `merman` does: a field that starts with `"` is
/// quoted and doubles its quotes, any other runs to the next comma, and every
/// field is trimmed and has `""` read as `"` afterwards.
fn parse_record(line: &str) -> Result<SankeyFlow> {
    let line = line.trim_end();
    let refuse = || {
        anyhow!(i18n::t!(
            "This line could not be read as a flow of a Sankey diagram ({}). Write it as `source,target,value`.",
            line.trim()
        ))
    };
    let (source, rest) = read_field(line).ok_or_else(refuse)?;
    let rest = rest.strip_prefix(',').ok_or_else(refuse)?;
    let (target, rest) = read_field(rest).ok_or_else(refuse)?;
    let rest = rest.strip_prefix(',').ok_or_else(refuse)?;
    let (value, rest) = read_field(rest).ok_or_else(refuse)?;
    if !rest.is_empty() {
        bail!(i18n::t!(
            "A line of the Sankey diagram has a fourth column ({}). Write only the three columns `source,target,value`.",
            line.trim()
        ));
    }
    let value = value.trim().to_string();
    if !is_valid_sankey_value(&value) {
        bail!(i18n::t!(
            "The structured editor cannot handle the amount “{value}” ({}). Write a number of 0 or more.",
            line.trim()
        ));
    }
    Ok(SankeyFlow {
        source: normalize_name(&source),
        target: normalize_name(&target),
        value,
    })
}

fn read_field(text: &str) -> Option<(String, &str)> {
    let Some(quoted) = text.strip_prefix('"') else {
        let end = text.find(',').unwrap_or(text.len());
        return Some((text.get(..end)?.to_string(), text.get(end..)?));
    };
    let mut out = String::new();
    let mut characters = quoted.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if character != '"' {
            out.push(character);
            continue;
        }
        if characters.peek().is_some_and(|(_, next)| *next == '"') {
            characters.next();
            out.push('"');
            continue;
        }
        return Some((out, quoted.get(offset + 1..)?));
    }
    // A quoted field that runs on to the next line is valid CSV, but `merman`
    // folds the blank lines inside it away and this module reads line by line,
    // so the record is refused rather than guessed at.
    None
}

/// `merman` cuts a `%%{…}%%` directive out of its line and reads whatever
/// follows it there as a statement, so keeping the line as a comment would hide
/// that statement from the form while it still draws.
pub(crate) fn refuse_statement_after_directive(text: &str) -> Result<()> {
    let Some(end) = text.rfind("}%%") else {
        return Ok(());
    };
    if text
        .get(end + "}%%".len()..)
        .is_some_and(|tail| !tail.trim().is_empty())
    {
        bail!(i18n::t!(
            "A statement follows `%%{{…}}%%` on the same line ({}). Put the directive and the statement on separate lines, then open the editor.",
            text.trim()
        ));
    }
    Ok(())
}

fn normalize_name(field: &str) -> String {
    field.trim().replace("\"\"", "\"")
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_sankey(document: &SankeyDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(document.keyword);
    out.push('\n');
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    for flow in &document.flows {
        let source = write_field(&flow.source);
        // Only a field that starts the record right at the `"` is read as a
        // quoted one, so a record that opens with a quote cannot be indented.
        if !source.starts_with('"') {
            out.push_str(INDENT);
        }
        out.push_str(&format!(
            "{source},{},{}\n",
            write_field(&flow.target),
            flow.value
        ));
    }
    for line in preserved_at(document, PreservedPosition::Bottom) {
        out.push_str(line);
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn preserved_at(
    document: &SankeyDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// A name as a CSV field: bare when `merman` reads it back unchanged, quoted
/// otherwise. `%%` at the start of a record would make the line a comment.
fn write_field(name: &str) -> String {
    if name.contains([',', '"']) || name.starts_with("%%") {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_string()
    }
}

/// Whether `name` can be written as a node of a Sankey diagram.
///
/// `merman` trims every field and reads `""` as `"` after unquoting it, so a
/// name with blanks at either end or two quotes in a row cannot be written.
pub fn is_valid_sankey_node(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name.contains(['\n', '\r'])
        && !name.contains("\"\"")
}

/// Whether `text` can be written as the value of a flow: a non-negative number
/// that `merman` reads as one. A negative width has no meaning in the drawing.
pub fn is_valid_sankey_value(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    (first.is_ascii_digit() || first == '.')
        && text.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, '.' | 'e' | 'E' | '+' | '-')
        })
        && text
            .parse::<f64>()
            .is_ok_and(|value| value.is_finite() && value >= 0.0)
}

/// [`crate::merman_fixtures`] as `(file name, source)` pairs.
#[cfg(test)]
pub(crate) fn merman_fixtures(kind: &str) -> Vec<(String, String)> {
    crate::merman_fixtures(kind)
        .into_iter()
        .map(|(path, source)| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            (name, source)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str = "sankey-beta\n    %% 1 行が 流れ元,流れ先,量。名前は半角英数字で書く (Mermaid は日本語の名前を読めない)\n    Power,Home,30\n    Power,Factory,50\n    Gas,Home,20\n    Gas,Factory,10";

    fn assert_stable(source: &str) -> String {
        let first =
            parse_sankey(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_sankey(&first);
        let second = parse_sankey(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_sankey(&second);
        assert_eq!(first, second, "the document changed through:\n{first_text}");
        assert_eq!(first_text, second_text);
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"));
        assert!(!svg.contains("Syntax error in text"), "{source}");
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in ["sankey-beta\n    A,B,1", "sankey\n    A,B,1"] {
            assert!(
                is_sankey_source(source),
                "{source} should be a Sankey diagram"
            );
        }
        for source in [
            "sankeys\nA,B,1",
            "Sankey-beta",
            "sankey-betas",
            "",
            "%% sankey",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_sankey_source(source),
                "{source} should not be a Sankey diagram"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse_sankey(TEMPLATE).expect("the template should parse");
        assert_eq!(
            document.node_names(),
            vec!["Power", "Home", "Factory", "Gas"]
        );
        assert_eq!(
            document
                .flows
                .iter()
                .map(|flow| flow.value.as_str())
                .collect::<Vec<_>>(),
            vec!["30", "50", "20", "10"]
        );
        assert_eq!(emit_sankey(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn quoting_follows_what_merman_reads() {
        let source = "sankey-beta\n\"District heating\",\"Heating and cooling, commercial\",22.505\n\"\"\"Biomass imports\"\"\",Solid,35\n    Agricultural 'waste',Bio-conversion,124.729   \n\"sankey\",target,10";
        let document = parse_sankey(source).expect("source should parse");
        assert_eq!(
            document
                .flows
                .iter()
                .map(|flow| (flow.source.as_str(), flow.target.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("District heating", "Heating and cooling, commercial"),
                ("\"Biomass imports\"", "Solid"),
                ("Agricultural 'waste'", "Bio-conversion"),
                ("sankey", "target"),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "sankey-beta\n    District heating,\"Heating and cooling, commercial\",22.505\n\"\"\"Biomass imports\"\"\",Solid,35\n    Agricultural 'waste',Bio-conversion,124.729\n    sankey,target,10"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn an_indented_quote_is_part_of_the_name() {
        // `merman` reads the quotes of an indented field as text, so the node is
        // `"A"` with its quotes, and writing it back has to keep them.
        let source = "sankey-beta\n    \"A\",B,1";
        let document = parse_sankey(source).expect("source should parse");
        assert_eq!(
            document.flows.first().map(|flow| flow.source.as_str()),
            Some("\"A\"")
        );
        let emitted = assert_stable(source);
        assert_eq!(emitted, "sankey-beta\n\"\"\"A\"\"\",B,1");
    }

    #[test]
    fn names_that_need_quotes_survive_a_round_trip() {
        for name in [
            "カンマ, あり",
            "引用 \" あり",
            "%% 先頭がコメント記号",
            "日本語",
            "末尾が\"",
            "\"",
        ] {
            let document = SankeyDocument {
                keyword: "sankey-beta",
                flows: vec![SankeyFlow {
                    source: name.to_string(),
                    target: "先".to_string(),
                    value: "1".to_string(),
                }],
                preserved_lines: Vec::new(),
            };
            assert_eq!(document.structure_error(), None, "{name}");
            let emitted = emit_sankey(&document);
            let parsed = parse_sankey(&emitted)
                .unwrap_or_else(|error| panic!("`{name}` should parse: {error}\n{emitted}"));
            assert_eq!(parsed, document, "{emitted}");
            assert_renders(&emitted);
        }
    }

    #[test]
    fn comments_keep_their_side_of_the_first_flow() {
        let source = "---\ntitle: 流れ\n---\n%%{init: {\"theme\": \"dark\"}}%%\nsankey-beta\n    %% 先頭\n    A,B,1\n    %% 途中\n    B,C,1\n    %% 末尾";
        let document = parse_sankey(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| (line.position, line.text.trim()))
                .collect::<Vec<_>>(),
            vec![
                (PreservedPosition::Preamble, "---\ntitle: 流れ\n---"),
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%"
                ),
                (PreservedPosition::Top, "%% 先頭"),
                (PreservedPosition::Bottom, "%% 途中"),
                (PreservedPosition::Bottom, "%% 末尾"),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 流れ\n---\n%%{init: {\"theme\": \"dark\"}}%%\nsankey-beta\n    %% 先頭\n    A,B,1\n    B,C,1\n    %% 途中\n    %% 末尾"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn renaming_a_node_follows_into_every_flow() {
        let mut document = parse_sankey(TEMPLATE).expect("the template should parse");
        document.rename_nodes(&[
            ("Power".to_string(), "Electric".to_string()),
            ("Home".to_string(), "Factory".to_string()),
            ("Factory".to_string(), "Home".to_string()),
        ]);
        assert_eq!(
            emit_sankey(&document),
            "sankey-beta\n    %% 1 行が 流れ元,流れ先,量。名前は半角英数字で書く (Mermaid は日本語の名前を読めない)\n    Electric,Factory,30\n    Electric,Home,50\n    Gas,Factory,20\n    Gas,Home,10"
        );
        assert_eq!(
            document.node_names(),
            vec!["Electric", "Factory", "Home", "Gas"]
        );
    }

    #[test]
    fn a_loop_and_bad_values_are_refused() {
        let mut document = parse_sankey(TEMPLATE).expect("the template should parse");
        document.flows.push(SankeyFlow {
            source: "Factory".to_string(),
            target: "Power".to_string(),
            value: "1".to_string(),
        });
        let error = document
            .structure_error()
            .expect("a loop should be refused");
        assert!(error.contains("Power → Factory → Power"), "{error}");
        // The reason still holds: `merman` gives up on the loop.
        let looped = render_to_svg(&emit_sankey(&document), &MermaidTheme::default());
        assert!(
            looped.is_err() || looped.is_ok_and(|svg| svg.contains("Syntax error in text")),
            "merman should not draw a loop"
        );

        let mut document = parse_sankey(TEMPLATE).expect("the template should parse");
        document.flows.push(SankeyFlow {
            source: "Home".to_string(),
            target: "Home".to_string(),
            value: "1".to_string(),
        });
        assert!(document.structure_error().is_some(), "a self loop");

        for value in ["", "-1", "abc", "1,5", "inf", "NaN", " 1", "+1", "１"] {
            assert!(!is_valid_sankey_value(value), "{value:?}");
        }
        for value in ["0", "1", "0.597", ".5", "1.", "1e3", "12345"] {
            assert!(is_valid_sankey_value(value), "{value:?}");
        }
        for name in ["", " A", "A ", "a\"\"b", "改\n行"] {
            assert!(!is_valid_sankey_node(name), "{name:?}");
        }
    }

    #[test]
    fn what_cannot_be_held_is_refused_with_a_japanese_reason() {
        let _japanese = crate::mermaid_model::japanese_for_this_thread();
        for source in [
            "sankey-beta\n    A,B",
            "sankey-beta\n    A,B,1,2",
            "sankey-beta\n    A,B,abc",
            "sankey-beta\n    A,B,-1",
            "sankey-beta\n\"A\nB\",C,1",
            "sankey-beta\n    A,A,1",
            "sankey-beta\n    A,B,1\n    B,A,1",
            "sankey-beta extra\n    A,B,1",
            "sankey-beta\n\"A\" ,B,1",
            "sankey-beta\n    A,B,1\n%%\n    B,C,1",
        ] {
            let error = parse_sankey(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.chars().any(|character| character > '\u{7f}'),
                "the message should be Japanese for `{source}`: {message}"
            );
        }
    }

    /// Two names that `merman` sanitizes into one node are one node in the
    /// drawing but two in the form, so the fence is not opened.
    #[test]
    fn names_merman_merges_are_refused() {
        // `merman` drops the script, so both records start at the node `x`.
        let source = "sankey-beta\n    x,C,1\n    x<script>a</script>,D,1";
        let error = parse_sankey(source)
            .err()
            .expect("merged names should be refused");
        assert!(format!("{error}").contains("as the same node"), "{error}");

        // The form can type the same names, and the confirm button's last
        // check reads the written source with this same function.
        let document = SankeyDocument {
            keyword: "sankey-beta",
            flows: vec![
                SankeyFlow {
                    source: "A".to_string(),
                    target: "B<iframe></iframe>".to_string(),
                    value: "1".to_string(),
                },
                SankeyFlow {
                    source: "B".to_string(),
                    target: "A".to_string(),
                    value: "1".to_string(),
                },
            ],
            preserved_lines: Vec::new(),
        };
        assert_eq!(document.structure_error(), None);
        assert!(parse_sankey(&emit_sankey(&document)).is_err());
    }

    /// `merman` reads the statement after a directive on the same line, so the
    /// line cannot be kept as a comment the form does not show.
    #[test]
    fn a_statement_after_a_directive_on_its_line_is_refused() {
        use crate::packet_model::parse_packet;
        use crate::venn_model::parse_venn;

        let refusals = [
            parse_sankey("sankey-beta\n%%{init: {}}%%   A,B,1\n    B,C,2").err(),
            parse_sankey("sankey-beta\n    A,B,1\n%%{init: {\n}}%% B,C,2").err(),
            parse_packet("packet-beta\n%%{init: {}}%% 0-7: \"a\"").err(),
            parse_packet("packet-beta\n    0-7: \"a\"\n%%{init: {}}%% 8-15: \"b\"").err(),
            parse_venn("venn-beta\n%%{init: {}}%%   set C\n    set A").err(),
        ];
        for refusal in refusals {
            let error = refusal.expect("the hidden statement should be refused");
            assert!(format!("{error}").contains("directive"), "{error}");
        }

        let document = parse_sankey("sankey-beta\n%%{init: {}}%%\n    A,B,1")
            .expect("a directive alone on its line is kept");
        assert_eq!(document.flows.len(), 1);
        assert!(parse_packet("packet-beta\n    %%{init: {}}%%\n    0-7: \"a\"").is_ok());
        assert!(parse_venn("venn-beta\n    %%{init: {}}%%\n    set A").is_ok());
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_sankey("").expect("an empty fence should parse");
        assert_eq!(document, SankeyDocument::empty());
        assert_eq!(emit_sankey(&document), "sankey-beta");
        assert_eq!(
            parse_sankey("sankey-beta").expect("a bare header should parse"),
            SankeyDocument::empty()
        );
        let document = parse_sankey("sankey\n  A,B,1").expect("the short keyword");
        assert_eq!(emit_sankey(&document), "sankey\n    A,B,1");
    }

    #[test]
    fn crlf_is_tolerated() {
        let document =
            parse_sankey("sankey-beta\r\n    A,B,1\r\n    B,C,2\r\n").expect("source should parse");
        assert_eq!(emit_sankey(&document), "sankey-beta\n    A,B,1\n    B,C,2");
    }

    /// Every Sankey example `merman` is tested against: whatever opens has to
    /// come back unchanged from a second round trip, give `merman` the same
    /// flows and draw; whatever does not open has to say why in Japanese.
    #[test]
    fn merman_fixtures_round_trip_or_are_refused_in_japanese() {
        let fixtures = merman_fixtures("sankey");
        if fixtures.is_empty() {
            eprintln!("the merman checkout was not found; skipping its Sankey examples");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (name, source) in &fixtures {
            match parse_sankey(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_sankey(&document);
                    let again = parse_sankey(&emitted).unwrap_or_else(|error| {
                        panic!("{name}: emitted source should parse: {error}\n{emitted}")
                    });
                    assert_eq!(again, document, "{name}");
                    assert_eq!(emit_sankey(&again), emitted, "{name}");
                    assert_eq!(
                        merman_flows(source).ok(),
                        merman_flows(&emitted).ok(),
                        "{name}"
                    );
                    if !document.flows.is_empty() {
                        assert_renders(&emitted);
                    }
                }
                Err(error) => {
                    let message = format!("{error}");
                    assert!(
                        message.chars().any(|character| character > '\u{7f}'),
                        "{name}: {message}"
                    );
                    refused.push((name.clone(), message));
                }
            }
        }
        eprintln!(
            "sankey fixtures: {opened} opened, {} refused: {refused:#?}",
            refused.len()
        );
        assert!(opened > 0);
    }
}
