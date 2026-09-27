//! A round-trippable editing model for Mermaid packet diagrams.
//!
//! A packet diagram is a title and a run of bit fields, each written as a bit
//! range (`0-15: "…"`), a single bit (`16: "…"`) or a length (`+8: "…"`).
//! `merman` refuses a field that does not start right after the one before it,
//! so a field's position is not the model's to hold: [`PacketField`] keeps its
//! length and the way it was written, and the start of every field is worked
//! out from the lengths before it ([`PacketDocument::ranges`]). Moving, adding
//! or removing a field therefore renumbers the ones after it, and a gap or an
//! overlap cannot be written at all.
//!
//! A document is opened only when writing it back gives `merman` the same
//! packet the fence gave it, so a line this module reads differently from
//! `merman` is refused rather than rewritten.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};
use crate::sankey_model::refuse_statement_after_directive;

const HEADERS: &[&str] = &["packet-beta", "packet"];

/// The most bits a packet can hold before `merman` stops laying out rows and
/// silently drops the rest: 10 000 rows of the default 32 bits.
pub const MAX_PACKET_BITS: u64 = 10_000 * 32;

/// How a field's extent is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketNotation {
    /// `0-15`: the first and the last bit.
    Range,
    /// `16`: one bit. A field written this way that grows past one bit is
    /// written as a range.
    Single,
    /// `+8`: the number of bits, starting after the previous field.
    Length,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketField {
    /// How many bits the field spans. Always at least 1 in a document that
    /// [`PacketDocument::structure_error`] accepts.
    pub bits: u64,
    pub notation: PacketNotation,
    pub label: String,
}

/// An editable packet diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct PacketDocument {
    /// `packet-beta` or `packet`, whichever the fence was written with.
    pub keyword: &'static str,
    /// The title, without the `title` keyword. `Some("")` is a `title`
    /// statement with nothing after it.
    pub title: Option<String>,
    pub fields: Vec<PacketField>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl PacketDocument {
    /// A blank packet diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: "packet-beta",
            title: None,
            fields: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// The first and last bit of every field, in order. A field of no bits
    /// is given the one bit `merman` would refuse it for, so that the ranges
    /// after it stay readable while the user fixes it.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        let mut next = 0u64;
        self.fields
            .iter()
            .map(|field| {
                let start = next;
                let end = start.saturating_add(field.bits.max(1) - 1);
                next = end.saturating_add(1);
                (start, end)
            })
            .collect()
    }

    /// Why the document cannot be written back as a diagram Mermaid draws, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_packet_title(title)
        {
            return Some("タイトルに `%%` と改行は使えません (前後の空白も不可)。".to_string());
        }
        let mut total = 0u64;
        for field in &self.fields {
            if !is_valid_packet_label(&field.label) {
                return Some(format!(
                    "ラベル「{}」は使えません (`%%` と改行は不可)。",
                    field.label
                ));
            }
            if field.bits == 0 {
                return Some(format!(
                    "「{}」のビット数は 1 以上の整数で書いてください。",
                    field.label
                ));
            }
            total = total.saturating_add(field.bits);
        }
        if total > MAX_PACKET_BITS {
            return Some(format!(
                "ビット数の合計 ({total}) が多すぎます。Mermaid が描けるのは {MAX_PACKET_BITS} ビットまでです。"
            ));
        }
        None
    }
}

/// Whether `source` is a packet diagram: its first statement is `packet-beta` or `packet`.
pub fn is_packet_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_packet(source: &str) -> Result<PacketDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = PacketDocument {
        keyword: "packet-beta",
        title: None,
        fields: Vec::new(),
        preserved_lines: preamble.preserved,
    };
    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let original = merman_packet(source)?;

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    reject_trailing_comment(header)?;
    document.keyword = match HEADERS.iter().find(|keyword| **keyword == header) {
        Some(keyword) => keyword,
        None => bail!(
            "`{header}` はパケット図の見出しとして読めません (`packet-beta` だけを 1 行に書きます)。"
        ),
    };

    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    let mut next_bit = 0u64;
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
        reject_trailing_comment(trimmed)?;
        // `merman` takes these only on one line, so the whole statement is here.
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            document.preserved_lines.push(PreservedLine {
                position,
                text: line.trim_end().to_string(),
            });
            continue;
        }
        if let Some(rest) = keyword_tail(trimmed, "title") {
            if document.title.is_some() {
                bail!("`title` が 2 回書かれています。1 つにまとめてからもう一度開いてください。");
            }
            document.title = Some(rest.trim().to_string());
            continue;
        }
        let field = parse_field(trimmed, next_bit)?;
        next_bit = next_bit.saturating_add(field.bits);
        document.fields.push(field);
        position = PreservedPosition::Bottom;
    }

    if let Some(error) = document.structure_error() {
        bail!("{error}");
    }
    let emitted = emit_packet(&document);
    let written = merman_packet(&emitted).map_err(|error| {
        anyhow!(
            "書き戻した図を Mermaid が読めませんでした ({error})。テキストのまま編集してください。"
        )
    })?;
    if original != written {
        bail!(
            "この図は構造化エディターで読むと Mermaid と違うパケットになります。テキストのまま編集してください。"
        );
    }
    Ok(document)
}

/// What `merman` reads from `source`, in a form that can be compared.
fn merman_packet(source: &str) -> Result<serde_json::Value> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(
                "Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。"
            )
        })?
        .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
    let RenderSemanticModel::Packet(model) = parsed.model else {
        bail!("この図はパケット図ではありません。");
    };
    serde_json::to_value(&model).map_err(|error| anyhow!("{error}"))
}

/// `merman` cuts every line at the first `%%`, even inside a label, so a
/// trailing comment would disappear on the next emit.
fn reject_trailing_comment(statement: &str) -> Result<()> {
    if statement.contains("%%") {
        bail!(
            "文の末尾のコメントは構造化編集で失われます ({statement})。コメントを独立した行に移すか、テキストのまま編集してください。"
        );
    }
    Ok(())
}

fn parse_field(statement: &str, next_bit: u64) -> Result<PacketField> {
    let refuse = || {
        anyhow!(
            "パケット図の行として読めませんでした ({statement})。`0-15: \"ラベル\"` か `+8: \"ラベル\"` の形で書いてください。"
        )
    };
    let (notation, start, end, rest) = if let Some(after_plus) = statement.strip_prefix('+') {
        let (bits, rest) = read_number(after_plus.trim_start()).ok_or_else(refuse)?;
        if bits == 0 {
            bail!("ビット数 0 の項目は書けません ({statement})。");
        }
        (
            PacketNotation::Length,
            next_bit,
            next_bit.saturating_add(bits - 1),
            rest,
        )
    } else {
        let (start, rest) = read_number(statement).ok_or_else(refuse)?;
        let rest = rest.trim_start();
        match rest.strip_prefix('-') {
            Some(after_dash) => {
                let (end, rest) = read_number(after_dash.trim_start()).ok_or_else(refuse)?;
                (PacketNotation::Range, start, end, rest)
            }
            None => (PacketNotation::Single, start, start, rest),
        }
    };
    if end < start {
        bail!("終了ビットが開始ビットより前です ({statement})。");
    }
    if start != next_bit {
        bail!(
            "項目が前の項目に続いていません ({statement})。{next_bit} ビット目から始めてください。"
        );
    }
    let after_colon = rest
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(refuse)?
        .trim_start();
    let (label, tail) = read_quoted(after_colon).ok_or_else(refuse)?;
    if !tail.trim().is_empty() {
        return Err(refuse());
    }
    Ok(PacketField {
        bits: end - start + 1,
        notation,
        label,
    })
}

/// A decimal number the way `merman` reads one: digits with no leading zero.
fn read_number(text: &str) -> Option<(u64, &str)> {
    let length = text
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let digits = text.get(..length)?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    Some((digits.parse().ok()?, text.get(length..)?))
}

/// Reads a quoted label the way `merman` does, including its `\` escapes, and
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

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_packet(document: &PacketDocument) -> String {
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
    if let Some(title) = &document.title {
        out.push_str(INDENT);
        out.push_str("title");
        if !title.is_empty() {
            out.push(' ');
            out.push_str(title);
        }
        out.push('\n');
    }
    for (field, (start, end)) in document.fields.iter().zip(document.ranges()) {
        let extent = match field.notation {
            PacketNotation::Length => format!("+{}", field.bits.max(1)),
            PacketNotation::Single if start == end => start.to_string(),
            PacketNotation::Single | PacketNotation::Range => format!("{start}-{end}"),
        };
        out.push_str(&format!(
            "{INDENT}{extent}: \"{}\"\n",
            escape_label(&field.label)
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
    document: &PacketDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn escape_label(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Whether `label` can be written as a field label. `%%` starts a comment
/// wherever it appears on a packet line, including inside the quotes.
pub fn is_valid_packet_label(label: &str) -> bool {
    !label.contains("%%") && !label.contains(['\n', '\r'])
}

/// Whether `title` can be written as the diagram's title, which runs to the
/// end of its line.
pub fn is_valid_packet_title(title: &str) -> bool {
    !title.contains("%%") && !title.contains(['\n', '\r']) && title.trim() == title
}

/// Whether `text` is a field length the form can take: a whole number of bits,
/// at least one.
pub fn is_valid_packet_bits(text: &str) -> bool {
    parse_packet_bits(text).is_some()
}

/// The length `text` spells, when it is one [`is_valid_packet_bits`] accepts.
pub fn parse_packet_bits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>()
        .ok()
        .filter(|bits| (1..=MAX_PACKET_BITS).contains(bits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sankey_model::merman_fixtures;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str = "packet-beta\n    %% 開始ビット-終了ビット: \"項目名\"\n    title UDP ヘッダー\n    0-15: \"送信元ポート\"\n    16-31: \"宛先ポート\"\n    32-47: \"長さ\"\n    48-63: \"チェックサム\"\n    64-95: \"データ (可変長)\"";

    fn assert_stable(source: &str) -> String {
        let first =
            parse_packet(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_packet(&first);
        let second = parse_packet(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        assert_eq!(first, second, "the document changed through:\n{first_text}");
        assert_eq!(first_text, emit_packet(&second));
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"));
        assert!(!svg.contains("Syntax error in text"), "{source}");
    }

    fn field(bits: u64, notation: PacketNotation, label: &str) -> PacketField {
        PacketField {
            bits,
            notation,
            label: label.to_string(),
        }
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in ["packet-beta\n    0-15: \"a\"", "packet\n    0-15: \"a\""] {
            assert!(
                is_packet_source(source),
                "{source} should be a packet diagram"
            );
        }
        for source in [
            "packets\n  0-1: \"a\"",
            "Packet-beta",
            "packet-betas",
            "",
            "%% packet",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_packet_source(source),
                "{source} should not be a packet diagram"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse_packet(TEMPLATE).expect("the template should parse");
        assert_eq!(document.title.as_deref(), Some("UDP ヘッダー"));
        assert_eq!(
            document.ranges(),
            vec![(0, 15), (16, 31), (32, 47), (48, 63), (64, 95)]
        );
        assert_eq!(emit_packet(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_notation_keeps_its_spelling() {
        let source = "packet\n    0-7: \"範囲\"\n    8: \"1 ビット\"\n    +4: \"長さ\"\n    13-13: \"1 ビットの範囲\"\n    +1: 'シングル'";
        let document = parse_packet(source).expect("source should parse");
        assert_eq!(
            document.fields,
            vec![
                field(8, PacketNotation::Range, "範囲"),
                field(1, PacketNotation::Single, "1 ビット"),
                field(4, PacketNotation::Length, "長さ"),
                field(1, PacketNotation::Range, "1 ビットの範囲"),
                field(1, PacketNotation::Length, "シングル"),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "packet\n    0-7: \"範囲\"\n    8: \"1 ビット\"\n    +4: \"長さ\"\n    13-13: \"1 ビットの範囲\"\n    +1: \"シングル\""
        );
        assert_renders(&emitted);
    }

    /// Moving or resizing a field renumbers every field after it, so the ranges
    /// stay contiguous — the one thing `merman` insists on.
    #[test]
    fn moving_and_resizing_renumber_the_fields_after() {
        let mut document = parse_packet(TEMPLATE).expect("the template should parse");
        document.fields.swap(0, 4);
        if let Some(first) = document.fields.get_mut(1) {
            first.bits = 8;
        }
        let emitted = emit_packet(&document);
        assert_eq!(
            emitted,
            "packet-beta\n    %% 開始ビット-終了ビット: \"項目名\"\n    title UDP ヘッダー\n    0-31: \"データ (可変長)\"\n    32-39: \"宛先ポート\"\n    40-55: \"長さ\"\n    56-71: \"チェックサム\"\n    72-87: \"送信元ポート\""
        );
        assert_stable(&emitted);
        assert_renders(&emitted);

        // A single bit that grows is written as a range.
        let mut document = parse_packet("packet-beta\n    0: \"a\"\n    1: \"b\"").expect("parse");
        if let Some(first) = document.fields.first_mut() {
            first.bits = 3;
        }
        assert_eq!(
            emit_packet(&document),
            "packet-beta\n    0-2: \"a\"\n    3: \"b\""
        );
    }

    #[test]
    fn gaps_overlaps_and_empty_fields_are_refused_as_merman_refuses_them() {
        for source in [
            "packet-beta\n    0-15: \"a\"\n    17-31: \"b\"",
            "packet-beta\n    0-15: \"a\"\n    10-31: \"b\"",
            "packet-beta\n    1-15: \"a\"",
            "packet-beta\n    5-3: \"a\"",
            "packet-beta\n    +0: \"a\"",
            "packet-beta\n    00-3: \"a\"",
        ] {
            assert!(
                crate::render_to_svg(source, &MermaidTheme::default())
                    .map_or(true, |svg| svg.contains("Syntax error in text")),
                "merman should refuse `{source}` too"
            );
            let error = parse_packet(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(
                format!("{error}")
                    .chars()
                    .any(|character| character > '\u{7f}'),
                "{error}"
            );
        }
        let mut document = PacketDocument::empty();
        document.fields.push(field(0, PacketNotation::Length, "a"));
        assert!(document.structure_error().is_some());
        document.fields = vec![field(MAX_PACKET_BITS + 1, PacketNotation::Range, "a")];
        assert!(document.structure_error().is_some());
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_field() {
        let source = "---\ntitle: 前置き\n---\npacket-beta\n    %% 先頭\n    accTitle: 概要\n    0-7: \"a\"\n    accDescr: 説明\n    %% 末尾";
        let document = parse_packet(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| (line.position, line.text.trim()))
                .collect::<Vec<_>>(),
            vec![
                (PreservedPosition::Preamble, "---\ntitle: 前置き\n---"),
                (PreservedPosition::Top, "%% 先頭"),
                (PreservedPosition::Top, "accTitle: 概要"),
                (PreservedPosition::Bottom, "accDescr: 説明"),
                (PreservedPosition::Bottom, "%% 末尾"),
            ]
        );
        assert_renders(&assert_stable(source));
    }

    #[test]
    fn labels_with_quotes_and_backslashes_survive() {
        for label in [
            "引用 \" あり",
            "バックスラッシュ \\ あり",
            "'",
            "",
            "コロン: あり",
        ] {
            let document = PacketDocument {
                keyword: "packet-beta",
                title: None,
                fields: vec![field(4, PacketNotation::Range, label)],
                preserved_lines: Vec::new(),
            };
            let emitted = emit_packet(&document);
            let parsed = parse_packet(&emitted)
                .unwrap_or_else(|error| panic!("`{label}` should parse: {error}\n{emitted}"));
            assert_eq!(parsed, document);
            assert_renders(&emitted);
        }
        assert!(!is_valid_packet_label("50%% 完了"));
        assert!(!is_valid_packet_title("メモ %% あり"));
        assert!(is_valid_packet_bits("8"));
        for text in ["", "0", "-1", "1.5", "８", " 8"] {
            assert!(!is_valid_packet_bits(text), "{text:?}");
        }
    }

    #[test]
    fn what_cannot_be_held_is_refused_with_a_japanese_reason() {
        for source in [
            "packet-beta\n    0-7: \"a\" %% メモ",
            "packet-beta %% メモ\n    0-7: \"a\"",
            "packet-beta\n    title 一\n    title 二",
            "packet-beta\n    0-7: a",
            "packet-beta\n    0-7: \"a\" 余り",
            "packet-beta\n    titles x",
            "packet-beta\n    accDescr {\n    説明\n    }",
        ] {
            let error = parse_packet(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.chars().any(|character| character > '\u{7f}'),
                "the message should be Japanese for `{source}`: {message}"
            );
        }
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_packet("").expect("an empty fence should parse");
        assert_eq!(document, PacketDocument::empty());
        assert_eq!(emit_packet(&document), "packet-beta");
        assert_eq!(
            parse_packet("packet-beta").expect("a bare header"),
            PacketDocument::empty()
        );
        let document = parse_packet("packet-beta\n  title").expect("an empty title");
        assert_eq!(document.title.as_deref(), Some(""));
        assert_eq!(emit_packet(&document), "packet-beta\n    title");
    }

    #[test]
    fn merman_fixtures_round_trip_or_are_refused_in_japanese() {
        let fixtures = merman_fixtures("packet");
        if fixtures.is_empty() {
            eprintln!("the merman checkout was not found; skipping its packet examples");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (name, source) in &fixtures {
            match parse_packet(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_packet(&document);
                    let again = parse_packet(&emitted).unwrap_or_else(|error| {
                        panic!("{name}: emitted source should parse: {error}\n{emitted}")
                    });
                    assert_eq!(again, document, "{name}");
                    assert_eq!(emit_packet(&again), emitted, "{name}");
                    assert_eq!(
                        merman_packet(source).ok(),
                        merman_packet(&emitted).ok(),
                        "{name}"
                    );
                    assert_renders(&emitted);
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
            "packet fixtures: {opened} opened, {} refused: {refused:#?}",
            refused.len()
        );
        assert!(opened > 0);
    }
}
