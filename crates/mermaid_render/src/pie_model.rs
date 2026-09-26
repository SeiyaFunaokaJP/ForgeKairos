//! A round-trippable editing model for Mermaid pie charts.
//!
//! The shape of this module mirrors [`crate::flowchart_model`]: [`parse_pie`]
//! reads the body of a ```` ```mermaid ```` fence into [`PieDocument`], and
//! [`emit_pie`] writes it back. See [`crate::mermaid_model`] for the invariants
//! every diagram kind keeps.
//!
//! Two decisions are specific to pie charts:
//!
//! - A slice value is kept as the text the user typed rather than as an `f64`,
//!   so that opening the editor on `"a" : 35.0` does not rewrite it to `35`.
//!   [`PieSlice::value_number`] parses it for callers that need the number.
//! - `merman`'s pie parser cuts every line at the first `%%`, even inside a
//!   quoted label, and it drops a second slice with the same label. Neither is
//!   recoverable from its model, so [`parse_pie`] scans the source itself and
//!   only uses `merman` to decide whether the fence is a pie chart it can draw
//!   at all.

use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble, statement_keyword,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieSlice {
    pub label: String,
    /// The value as the source spells it, e.g. `35`, `35.0` or `.5`. Always a
    /// string [`is_valid_pie_value`] accepts.
    pub value: String,
}

impl PieSlice {
    pub fn value_number(&self) -> Option<f64> {
        self.value.parse::<f64>().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieDocument {
    /// The chart's title, without the `title` keyword. `Some("")` is a `title`
    /// statement with nothing after it, which Mermaid accepts.
    pub title: Option<String>,
    /// Whether the header carries `showData`, which draws each value next to
    /// its slice.
    pub show_data: bool,
    pub slices: Vec<PieSlice>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl PieDocument {
    /// A blank pie chart, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            show_data: false,
            slices: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    pub fn slice_index(&self, label: &str) -> Option<usize> {
        self.slices.iter().position(|slice| slice.label == label)
    }

    /// The sum of the slice values, which is what a percentage is measured
    /// against. Values that do not parse count as zero.
    pub fn total(&self) -> f64 {
        self.slices
            .iter()
            .filter_map(PieSlice::value_number)
            .filter(|value| value.is_finite())
            .sum()
    }
}

/// Whether `source` is a pie chart. Cheap enough to call before parsing.
pub fn is_pie_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, "pie").is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_pie(source: &str) -> Result<PieDocument> {
    if !source.trim().is_empty() {
        let engine = Engine::new();
        let parsed = engine
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::Pie(_)) {
            bail!("この図は円グラフではありません。");
        }
    }

    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = PieDocument {
        title: None,
        show_data: false,
        slices: Vec::new(),
        preserved_lines: preamble.preserved,
    };

    // An empty fence: the editor opens on a blank chart, and whatever comments
    // the user left keep their place.
    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, "pie") else {
        bail!("円グラフではありません (`pie` で始まる図だけを構造化編集できます)。");
    };
    reject_trailing_comment(header)?;
    parse_header_tail(tail, &mut document)?;

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
            document.preserved_lines.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        if is_accessibility_statement(trimmed) {
            if statement_keyword(trimmed, "accDescr").is_some_and(|rest| rest.starts_with('{'))
                && !trimmed.contains('}')
            {
                while lines.get(index).is_some_and(|line| !line.contains('}')) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            document.preserved_lines.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }

        reject_trailing_comment(trimmed)?;
        if let Some(rest) = keyword_tail(trimmed, "title") {
            if document.title.is_some() {
                bail!("`title` が 2 回書かれています。1 つにまとめてからもう一度開いてください。");
            }
            set_title(&mut document, rest)?;
            continue;
        }
        document.slices.push(parse_slice(trimmed)?);
        position = PreservedPosition::Bottom;
    }

    Ok(document)
}

/// Reads what follows `pie` on the header line.
///
/// `title` swallows the rest of the line, so `showData` is only a flag while it
/// comes first; anything else is a spelling this module would drop.
fn parse_header_tail(tail: &str, document: &mut PieDocument) -> Result<()> {
    let mut rest = tail.trim_start();
    loop {
        if rest.is_empty() {
            return Ok(());
        }
        if let Some(after) = keyword_tail(rest, "showData") {
            document.show_data = true;
            rest = after;
            continue;
        }
        if let Some(after) = keyword_tail(rest, "title") {
            set_title(document, after)?;
            return Ok(());
        }
        bail!(
            "`pie` の行の `{rest}` は構造化編集に対応していません。テキストのまま編集してください。"
        );
    }
}

fn set_title(document: &mut PieDocument, text: &str) -> Result<()> {
    let title = text.trim();
    if !is_valid_pie_title(title) {
        bail!("タイトル `{title}` は円グラフに書けません (`%%` は使えません)。");
    }
    document.title = Some(title.to_string());
    Ok(())
}

/// Whether `statement` is an accessibility statement, which the model does not
/// carry and which is therefore written back verbatim.
fn is_accessibility_statement(statement: &str) -> bool {
    statement_keyword(statement, "accTitle").is_some()
        || statement_keyword(statement, "accDescr").is_some()
}

/// `merman` reads a pie chart line by line and cuts each line at the first
/// `%%`, so a trailing comment would disappear on the next emit.
fn reject_trailing_comment(statement: &str) -> Result<()> {
    if statement.contains("%%") {
        bail!(
            "文の末尾のコメントは構造化編集で失われます ({statement})。コメントを独立した行に移すか、テキストのまま編集してください。"
        );
    }
    Ok(())
}

fn parse_slice(statement: &str) -> Result<PieSlice> {
    let Some((label, after_label)) = read_quoted(statement) else {
        bail!(
            "円グラフの行として読めませんでした ({statement})。`\"ラベル\" : 数値` の形で書いてください。"
        );
    };
    let Some(after_colon) = after_label.trim_start().strip_prefix(':') else {
        bail!("ラベルのあとに `:` がありません ({statement})。");
    };
    let rest = after_colon.trim_start();
    let value = rest
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || *character == '.' || *character == '-'
        })
        .collect::<String>();
    let remainder = rest.get(value.len()..).unwrap_or_default().trim();
    // `merman` reads the digits in front of the exponent and ignores the rest
    // of the line, so `1e3` is drawn as a slice of 1. Opening it with the
    // spelling kept would show the editor a thousand where the picture has one.
    if is_exponent(remainder) {
        bail!(
            "指数表記の値 ({value}{remainder}) は Mermaid が `{value}` と読むため、図と一致しません ({statement})。`1000` のように桁を書き下してから開いてください。"
        );
    }
    if !remainder.is_empty() {
        bail!(
            "数値のあとの `{remainder}` は円グラフに書けません ({statement})。テキストのまま編集してください。"
        );
    }
    if !is_valid_pie_value(&value) {
        bail!(
            "`{value}` は円グラフの値として読めません ({statement})。0 以上の数値を書いてください。"
        );
    }
    Ok(PieSlice { label, value })
}

fn is_exponent(text: &str) -> bool {
    let Some(exponent) = text.strip_prefix(['e', 'E']) else {
        return false;
    };
    let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
}

/// Reads a quoted string the way `merman` does, including its `\` escapes, and
/// returns it with the rest of the line.
fn read_quoted(text: &str) -> Option<(String, &str)> {
    let mut characters = text.char_indices();
    let (_, quote) = characters.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for (offset, character) in characters {
        if escaped {
            out.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            return Some((out, text.get(offset + character.len_utf8()..)?));
        }
        out.push(character);
    }
    None
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_pie(document: &PieDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    out.push_str("pie");
    if document.show_data {
        out.push_str(" showData");
    }
    if let Some(title) = &document.title {
        out.push_str(" title");
        if !title.is_empty() {
            out.push(' ');
            out.push_str(title);
        }
    }
    out.push('\n');

    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    for slice in &document.slices {
        let _ = writeln!(
            out,
            "{INDENT}\"{}\" : {}",
            escape_label(&slice.label),
            slice.value
        );
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

fn preserved_at(document: &PieDocument, position: PreservedPosition) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// A pie label is always quoted, and the only characters the parser cannot read
/// back are the quote itself and the backslash that escapes it.
fn escape_label(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Whether `label` can be written as a pie slice label.
///
/// `%%` starts a comment wherever it appears on a pie chart line, including
/// inside the quotes, and there is no escape for it.
pub fn is_valid_pie_label(label: &str) -> bool {
    !label.is_empty() && !label.contains("%%") && !label.contains(['\n', '\r'])
}

/// Whether `text` can be written as a pie slice value: a non-negative decimal
/// number, spelled the way `merman`'s number scanner reads one.
pub fn is_valid_pie_value(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
        && text.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Whether `title` can be written as a pie chart title. The title runs to the
/// end of its line, so only a comment marker is in the way.
pub fn is_valid_pie_title(title: &str) -> bool {
    !title.contains("%%") && !title.contains(['\n', '\r']) && title.trim() == title
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let first =
            parse_pie(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_pie(&first);
        let second = parse_pie(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_pie(&second);
        assert_eq!(
            first, second,
            "the document changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    const EXAMPLES: [&str; 8] = [
        "pie title 内訳\n    \"設計\" : 30\n    \"実装\" : 50\n    \"テスト\" : 20",
        "pie showData\n  \"A\" : 1\n  \"B\" : 2",
        "pie showData title 割合\n  \"あ\" : 12.5\n  \"い\" : 87.5",
        "pie\n\"one\" : 1",
        "pie title t\n  'single' : 1\n  \"double\" : 2",
        "pie title 小数\n  \"a\" : .5\n  \"b\" : 1.\n  \"c\" : 0",
        "pie title コメント入り\n  %% 先頭のメモ\n  \"a\" : 1\n  %% 末尾のメモ",
        "---\ntitle: フロントマター\n---\n%%{init: {\"theme\": \"dark\"}}%%\npie title 図\n  accTitle: 概要\n  \"a\" : 1",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_insert_template_round_trips() {
        let template = "pie title 内訳\n    \"設計\" : 30\n    \"実装\" : 50\n    \"テスト\" : 20";
        let document = parse_pie(template).expect("the template should parse");
        assert_eq!(document.title.as_deref(), Some("内訳"));
        assert!(!document.show_data);
        assert_eq!(
            document
                .slices
                .iter()
                .map(|slice| (slice.label.as_str(), slice.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("設計", "30"), ("実装", "50"), ("テスト", "20")]
        );
        assert_eq!(emit_pie(&document), template);
    }

    #[test]
    fn a_value_keeps_the_spelling_it_was_written_with() {
        let document = parse_pie("pie\n  \"a\" : 35.0\n  \"b\" : 5").expect("source should parse");
        assert_eq!(
            document
                .slices
                .iter()
                .map(|slice| slice.value.clone())
                .collect::<Vec<_>>(),
            vec!["35.0".to_string(), "5".to_string()]
        );
        assert_eq!(
            document.slices.first().and_then(PieSlice::value_number),
            Some(35.0)
        );
        assert_eq!(document.total(), 40.0);
        assert!(emit_pie(&document).contains("35.0"));
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_slice() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\npie title 図\n  %% 先頭のメモ\n  \"a\" : 1\n  accDescr {\n    説明の 1 行目\n    2 行目\n  }";
        let document = parse_pie(source).expect("source should parse");
        let positions = document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.trim().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (
                    PreservedPosition::Bottom,
                    "accDescr {\n    説明の 1 行目\n    2 行目\n  }".to_string()
                ),
            ]
        );

        let emitted = assert_stable(source);
        assert!(
            emitted
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with("%%{init"),
            "the init directive must stay before the diagram keyword:\n{emitted}"
        );
        assert!(
            emitted.contains("説明の 1 行目"),
            "the multi-line accDescr block must survive:\n{emitted}"
        );
    }

    #[test]
    fn labels_with_quotes_and_multibyte_text_survive() {
        for label in [
            "引用 \" あり",
            "バックスラッシュ \\ あり",
            "コロン : あり",
            "セミコロン ; あり",
            "かっこ (まる) [かく]",
            "日本語のラベル",
            "50% 達成",
        ] {
            let document = PieDocument {
                title: None,
                show_data: false,
                slices: vec![PieSlice {
                    label: label.to_string(),
                    value: "1".to_string(),
                }],
                preserved_lines: Vec::new(),
            };
            let emitted = emit_pie(&document);
            let parsed = parse_pie(&emitted)
                .unwrap_or_else(|error| panic!("`{label}` should parse: {error}\n{emitted}"));
            assert_eq!(
                parsed.slices.first().map(|slice| slice.label.as_str()),
                Some(label),
                "label of {emitted}"
            );
            assert_renders(&emitted);
        }
    }

    #[test]
    fn a_duplicate_label_is_not_dropped_from_the_text() {
        // `merman` draws only the first slice of a repeated label. Keeping both
        // rows is what the fence says, and it lets the editor fix the mistake.
        let source = "pie\n  \"a\" : 1\n  \"a\" : 2";
        let document = parse_pie(source).expect("source should parse");
        assert_eq!(document.slices.len(), 2);
        assert_eq!(document.slice_index("a"), Some(0));
        assert_eq!(document.slice_index("b"), None);
        assert_eq!(assert_stable(source), "pie\n    \"a\" : 1\n    \"a\" : 2");
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_chart() {
        let document = parse_pie("").expect("an empty fence should parse");
        assert_eq!(document, PieDocument::empty());
        assert_eq!(emit_pie(&document), "pie");
        assert_eq!(emit_pie(&PieDocument::empty()), "pie");
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for source in [
            "pie title 図\n  \"a\" : 1e3",
            "pie title 図\n  \"a\" : 1 個",
            "pie title 図\n  title 二つ目\n  \"a\" : 1",
            "pie accTitle: 概要\n  \"a\" : 1",
            "pie title 図\n  a : 1",
        ] {
            let error = parse_pie(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.chars().any(|character| character > '\u{7f}'),
                "the message should be Japanese for `{source}`: {message}"
            );
        }

        // A comment is what the user has to move, so the reason has to say so
        // rather than complain about whatever the comment made unreadable.
        for source in [
            "pie title 図\n  \"a\" : 1 %% メモ",
            "pie title 図 %% メモ\n  \"a\" : 1",
            "pie\n  title 独立行 %% メモ\n  \"a\" : 1",
        ] {
            let error = parse_pie(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains("コメント") || message.contains("`%%`"),
                "the reason for `{source}` should name the comment: {message}"
            );
        }
    }

    /// `merman` draws `1e3` as a slice of 1, so opening it would put a value
    /// in the form that the picture does not show. The refusal has to say why,
    /// and the reason has to still hold: if `merman` ever reads the exponent,
    /// the first assertion fails and the value can be opened after all.
    #[test]
    fn an_exponent_is_refused_because_mermaid_reads_only_its_digits() {
        let svg = render_to_svg(
            "pie showData\n  \"a\" : 1e3\n  \"b\" : 1",
            &MermaidTheme::default(),
        )
        .expect("merman should draw the chart");
        assert!(svg.contains("a [1]"), "{svg}");
        assert!(!svg.contains("a [1000]"), "{svg}");

        for source in [
            "pie\n  \"a\" : 1e3",
            "pie\n  \"a\" : 1.5E+2",
            "pie\n  \"a\" : 2e-1",
        ] {
            let error = parse_pie(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(format!("{error}").contains("指数表記"), "{error}");
        }
        // A word after the number is still refused for what it is.
        let error = parse_pie("pie\n  \"a\" : 1 each")
            .err()
            .expect("a trailing word should be refused");
        assert!(!format!("{error}").contains("指数表記"), "{error}");
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "flowchart TD\n  A --> B",
            "stateDiagram-v2\n  [*] --> A",
            "sequenceDiagram\n  A->>B: hi",
        ] {
            assert!(!is_pie_source(source), "{source}");
            assert!(parse_pie(source).is_err(), "{source}");
        }
        assert!(is_pie_source("pie title x\n  \"a\" : 1"));
        assert!(is_pie_source("%% メモ\npie\n  \"a\" : 1"));
        assert!(is_pie_source("---\ntitle: x\n---\npie\n  \"a\" : 1"));
        assert!(!is_pie_source("pieces\n  \"a\" : 1"));
    }

    #[test]
    fn values_and_labels_are_validated() {
        assert!(is_valid_pie_value("1"));
        assert!(is_valid_pie_value("35.0"));
        assert!(is_valid_pie_value(".5"));
        assert!(!is_valid_pie_value(""));
        assert!(!is_valid_pie_value("-1"));
        assert!(!is_valid_pie_value("1e3"));
        assert!(!is_valid_pie_value("1.2.3"));
        assert!(!is_valid_pie_value("１"));

        assert!(is_valid_pie_label("設計"));
        assert!(is_valid_pie_label("引用 \" あり"));
        assert!(!is_valid_pie_label(""));
        assert!(!is_valid_pie_label("100%% 完了"));
        assert!(is_valid_pie_label(" 前後の空白 "));

        assert!(is_valid_pie_title("内訳"));
        assert!(is_valid_pie_title(""));
        assert!(!is_valid_pie_title("メモ %% あり"));
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "pie title 図  \r\n    \"a\" : 1   \r\n    \"b\" : 2\r\n";
        let document = parse_pie(source).expect("source should parse");
        assert_eq!(document.title.as_deref(), Some("図"));
        assert_eq!(document.slices.len(), 2);
        assert_eq!(
            emit_pie(&document),
            "pie title 図\n    \"a\" : 1\n    \"b\" : 2"
        );
    }
}
