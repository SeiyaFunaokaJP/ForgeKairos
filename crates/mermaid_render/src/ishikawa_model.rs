//! A round-trippable editing model for Mermaid Ishikawa (fishbone) diagrams.
//!
//! [`parse_ishikawa`] reads the body of a ```` ```mermaid ```` fence into
//! [`IshikawaDocument`], and [`emit_ishikawa`] writes it back. See
//! [`crate::mermaid_model`] for the invariants every diagram kind keeps.
//!
//! The first line after the keyword is the effect — the head of the fish — and
//! every line after it is a cause, nested by indentation: the first cause's
//! indentation is the level of the main bones, and a cause hangs under the
//! last one above it that is indented less. A cause indented less than the
//! first one is still a main bone, which is how `merman` lets the effect be
//! indented deeper than the causes. The model keeps the effect and the causes
//! as an outline — each cause with its depth, 1 for a main bone — and forgets
//! the widths, so the emitter writes the effect four spaces in and every level
//! four spaces deeper than its parent.
//!
//! What this module refuses rather than rewrites, all of it with the reason in
//! Japanese:
//!
//! - `%%` right after the keyword, which `merman` reads as the effect's text
//!   but which would turn into a comment on a line of its own;
//! - `%%{` inside a line, which Mermaid's preprocessing reads as the start of a
//!   directive and cuts out together with everything up to the next `}%%`;
//! - anything else `merman` does not read as an Ishikawa diagram.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, an effect written on the keyword line (it moves to a line of
//! its own) and the position of comment lines between causes, which move
//! below the tree the way they do in [`crate::mindmap_model`]. `merman` has no
//! `title` or `accTitle` statements here: such a line is a cause like any
//! other, and it is kept as one.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail_ignoring_case, scan_preamble,
};
use crate::mindmap_model::OutlineItem;
use crate::tree_view_model::text_after_directive;

const HEADERS: &[&str] = &["ishikawa-beta", "ishikawa"];

/// One cause, in the order the fence writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IshikawaCause {
    /// 1 for a main bone (a category of causes), 2 for a cause on it and so
    /// on.
    pub depth: usize,
    pub text: String,
}

impl IshikawaCause {
    /// A main bone.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            depth: 1,
            text: text.into(),
        }
    }
}

impl OutlineItem for IshikawaCause {
    fn depth(&self) -> usize {
        self.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }
}

/// An editable Ishikawa diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IshikawaDocument {
    /// The keyword as the fence spells it: `ishikawa-beta` or `ishikawa`, in
    /// any case.
    pub keyword: String,
    /// The effect, the problem the causes lead to. `None` for a fence with
    /// nothing but the keyword.
    pub effect: Option<String>,
    pub causes: Vec<IshikawaCause>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl IshikawaDocument {
    /// A blank Ishikawa diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: HEADERS[0].to_string(),
            effect: None,
            causes: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        match &self.effect {
            None if !self.causes.is_empty() => {
                return Some(
                    "結果 (問題) が空です。要因を書くには、先に結果を入力してください。"
                        .to_string(),
                );
            }
            Some(effect) if !is_valid_ishikawa_text(effect) => {
                return Some(format!("結果 `{effect}` は書けません ({TEXT_RULES})。"));
            }
            _ => {}
        }
        let mut previous_depth = 0;
        for cause in &self.causes {
            if cause.depth == 0 || cause.depth > previous_depth + 1 {
                return Some(format!(
                    "要因 `{}` が親より 2 段以上深くなっています。",
                    cause.text
                ));
            }
            if !is_valid_ishikawa_text(&cause.text) {
                return Some(if cause.text.trim().is_empty() {
                    "空の要因があります。".to_string()
                } else {
                    format!("要因 `{}` は書けません ({TEXT_RULES})。", cause.text)
                });
            }
            previous_depth = cause.depth;
        }
        None
    }
}

const TEXT_RULES: &str = "改行と `%%{` は使えず、`%%` で始めることや前後の空白もできません";

/// Whether `source` is an Ishikawa diagram: its first statement is `ishikawa-beta` or `ishikawa`.
///
/// Unlike most diagrams, `merman` takes this keyword in any case.
pub fn is_ishikawa_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail_ignoring_case(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_ishikawa(source: &str) -> Result<IshikawaDocument> {
    let document = scan_ishikawa(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model(), RenderSemanticModel::Ishikawa(_)) {
            bail!("この図は特性要因図ではありません。");
        }
    }

    Ok(document)
}

fn scan_ishikawa(source: &str) -> Result<IshikawaDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(IshikawaDocument {
            preserved_lines,
            ..IshikawaDocument::empty()
        });
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some((keyword, tail)) = HEADERS.iter().find_map(|candidate| {
        let tail = keyword_tail_ignoring_case(header, candidate)?;
        Some((header.get(..candidate.len())?, tail.trim_end()))
    }) else {
        bail!(
            "特性要因図ではありません (`ishikawa-beta` か `ishikawa` で始まる図だけを構造化編集できます)。"
        );
    };
    if tail.starts_with("%%") {
        bail!(
            "`{keyword}` の行の `{tail}` は Mermaid が結果の文字として読みますが、書き戻すとコメントになります。テキストのまま編集してください。"
        );
    }
    if tail.contains("%%{") {
        bail!(
            "`%%{{` を含む行は Mermaid が指示子として切り取るので、構造化編集に対応していません ({tail})。"
        );
    }
    let mut effect = (!tail.is_empty()).then(|| tail.to_string());

    // Each cause with the indentation it was written at, which only says
    // something relative to the causes around it.
    let mut written: Vec<(usize, String)> = Vec::new();
    let mut index = header_index + 1;
    let mut position = if effect.is_some() {
        PreservedPosition::Bottom
    } else {
        PreservedPosition::Top
    };
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
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
            if let Some(rest) = text_after_directive(&text) {
                bail!(
                    "指示子 `%%{{…}}%%` の後ろの `{rest}` は Mermaid が要因として読みますが、構造化編集ではコメントと一緒に動いてしまいます。指示子を独立した行にしてください。"
                );
            }
            preserved_lines.push(PreservedLine { position, text });
            continue;
        }
        if trimmed.contains("%%{") {
            bail!(
                "`%%{{` を含む行は Mermaid が指示子として切り取るので、構造化編集に対応していません ({trimmed})。"
            );
        }

        // `merman` counts only spaces and tabs as indentation, one column
        // each, and trims any other whitespace off the text.
        let rest = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - rest.len();
        if effect.is_none() {
            effect = Some(trimmed.to_string());
            position = PreservedPosition::Bottom;
        } else {
            written.push((indent, trimmed.to_string()));
        }
    }

    // A cause indented less than the first one is a main bone all the same,
    // so its indentation counts as the first one's.
    let base = written.first().map_or(0, |(indent, _)| *indent);
    let mut open_indents: Vec<usize> = Vec::new();
    let mut causes = Vec::with_capacity(written.len());
    for (indent, text) in written {
        let indent = indent.max(base);
        while open_indents.last().is_some_and(|open| *open >= indent) {
            open_indents.pop();
        }
        causes.push(IshikawaCause {
            depth: open_indents.len() + 1,
            text,
        });
        open_indents.push(indent);
    }

    Ok(IshikawaDocument {
        keyword: keyword.to_string(),
        effect,
        causes,
        preserved_lines,
    })
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_ishikawa(document: &IshikawaDocument) -> String {
    let mut out = String::new();
    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&document.keyword);
    out.push('\n');
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(effect) = &document.effect {
        out.push_str(INDENT);
        out.push_str(effect);
        out.push('\n');
    }
    for cause in &document.causes {
        out.push_str(&INDENT.repeat(cause.depth + 1));
        out.push_str(&cause.text);
        out.push('\n');
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
    document: &IshikawaDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `text` can be written as the effect or a cause and read back as
/// itself. A line starting with `%%` is a comment, and `merman` trims the text,
/// so whitespace around it cannot be written.
pub fn is_valid_ishikawa_text(text: &str) -> bool {
    !text.trim().is_empty()
        && text.trim() == text
        && !text.contains(['\n', '\r'])
        && !text.starts_with("%%")
        && !text.contains("%%{")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mindmap_model::{
        outline_add_child, outline_add_sibling, outline_indent, outline_move, outline_outdent,
        outline_parent, outline_remove,
    };
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    const TEMPLATE: &str = "ishikawa-beta\n    %% 最初の行が結果、字下げした行が要因\n    不良品が多い\n        人\n            経験が浅い\n        方法\n            手順書が古い\n        材料\n            仕入れ先のばらつき";

    /// Every distinct diagram among the examples `merman` ships for Ishikawa
    /// diagrams (its `fixtures/ishikawa`); the others differ only in their
    /// frontmatter, one of which is kept here.
    const MERMAN_EXAMPLES: &[&str] = &[
        "---\nconfig:\n  ishikawa:\n    useMaxWidth: true\n---\nishikawa-beta\n    Blurry Photo\n        Process\n            Out of focus\n        User\n            Shaky hands\n",
        "---\nconfig:\n  theme: dark\n---\nishikawa-beta\n    Blurry Photo\n        Process\n            Out of focus\n        User\n            Shaky hands\n",
        "ishikawa-beta\n    Problem\nCause A\n  Subcause A1\n  Subcause A2\nCause B\n  Subcause B1\nCause C\n",
        "ishikawa-beta\n    Blurry Photo\n        Process\n            Out of focus\n        User\n            Shaky hands\n",
        "ishikawa-beta\n    Manufacturing Defect\n        Machine\n            Worn tooling\n            Calibration\n        Method\n            Missing step\n        Material\n            Contamination\n            Wrong grade\n        Manpower\n            Insufficient training\n",
        "ishikawa-beta\n    Server Outage\n        Hardware\n            Disk\n                Bad sectors\n                Full capacity\n            Memory\n                Leak detected\n        Software\n            Bug\n                Race condition\n",
        "ishikawa-beta\n    Problem\n        Cause A\n",
        "ishikawa-beta\n    Problem\n",
        "ishikawa-beta\n    Blurry Photo\n    Process\n        Out of focus\n        Shutter speed too slow\n        Protective film not removed\n        Beautification filter applied\n    User\n        Shaky hands\n    Equipment\n        LENS\n            Inappropriate lens\n            Damaged lens\n            Dirty lens\n        SENSOR\n            Damaged sensor\n            Dirty sensor\n    Environment\n        Subject moved too quickly\n        Too dark\n",
    ];

    fn parse(source: &str) -> IshikawaDocument {
        parse_ishikawa(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_ishikawa(&first);
        let second = parse(&first_text);
        let second_text = emit_ishikawa(&second);
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

    fn causes(document: &IshikawaDocument) -> Vec<(usize, &str)> {
        document
            .causes
            .iter()
            .map(|cause| (cause.depth, cause.text.as_str()))
            .collect()
    }

    /// The effect and each cause with its parent's text, the way `merman`
    /// builds the fish.
    fn merman_fish(source: &str) -> (Option<String>, Vec<(String, String)>) {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source:?}: {error}"))
            .unwrap_or_else(|| panic!("merman should find a diagram in {source:?}"));
        let RenderSemanticModel::Ishikawa(model) = parsed.into_parts().1 else {
            panic!("{source:?} should be an Ishikawa diagram");
        };
        let Some(root) = model.root else {
            return (None, Vec::new());
        };
        let mut edges = Vec::new();
        let mut pending = vec![&root];
        while let Some(node) = pending.pop() {
            for child in &node.children {
                edges.push((node.text.clone(), child.text.clone()));
                pending.push(child);
            }
        }
        edges.sort();
        (Some(root.text), edges)
    }

    fn model_fish(document: &IshikawaDocument) -> (Option<String>, Vec<(String, String)>) {
        let causes = &document.causes;
        let effect = document.effect.clone().unwrap_or_default();
        let mut edges = causes
            .iter()
            .enumerate()
            .map(|(index, cause)| {
                let parent = outline_parent(causes, index)
                    .and_then(|parent| causes.get(parent))
                    .map_or(effect.clone(), |parent| parent.text.clone());
                (parent, cause.text.clone())
            })
            .collect::<Vec<_>>();
        edges.sort();
        (document.effect.clone(), edges)
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in [
            "ishikawa-beta\n    問題",
            "ishikawa\n    問題",
            "Ishikawa-Beta\n    問題",
            "ISHIKAWA 問題",
        ] {
            assert!(
                is_ishikawa_source(source),
                "{source} should be a Ishikawa diagram"
            );
        }
        for source in [
            "ishikawas\n  問題",
            "ishikawa-betas",
            "ishikawa:",
            "",
            "%% ishikawa",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_ishikawa_source(source),
                "{source} should not be a Ishikawa diagram"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(document.effect.as_deref(), Some("不良品が多い"));
        assert_eq!(
            causes(&document),
            vec![
                (1, "人"),
                (2, "経験が浅い"),
                (1, "方法"),
                (2, "手順書が古い"),
                (1, "材料"),
                (2, "仕入れ先のばらつき"),
            ]
        );
        assert_eq!(document.structure_error(), None);
        assert_eq!(emit_ishikawa(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_merman_example_round_trips_and_renders() {
        for source in MERMAN_EXAMPLES {
            let document = parse(source);
            assert_eq!(document.structure_error(), None, "{source}");
            assert_eq!(model_fish(&document), merman_fish(source), "{source}");
            let emitted = assert_stable(source);
            assert_eq!(merman_fish(&emitted), merman_fish(source), "{emitted}");
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_depth_is_read_relative_to_the_first_cause() {
        // The first cause sets the main bones' level; `c` is shallower than
        // it and is a main bone all the same, and so is `d`, although it is
        // indented deeper than `c`.
        let source = "Ishikawa 結果\n      a\n          a1\n  c\n   d\n      b\n        b1\n             b2\n        b3";
        let document = parse(source);
        assert_eq!(document.keyword, "Ishikawa");
        assert_eq!(model_fish(&document), merman_fish(source));
        assert_eq!(
            causes(&document),
            vec![
                (1, "a"),
                (2, "a1"),
                (1, "c"),
                (1, "d"),
                (1, "b"),
                (2, "b1"),
                (3, "b2"),
                (2, "b3"),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "Ishikawa\n    結果\n        a\n            a1\n        c\n        d\n        b\n            b1\n                b2\n            b3"
        );
        assert_eq!(merman_fish(&emitted), merman_fish(source));
        assert_renders(&emitted);
    }

    #[test]
    fn comments_and_the_preamble_keep_their_side_of_the_fish() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nishikawa\n    %% 先頭のメモ\n    結果\n        人\n        %% 途中のメモ\n            未熟 %% 文字の一部\n        title は要因";
        let document = parse(source);
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
                    "---\ntitle: 図\n---".to_string()
                ),
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 途中のメモ".to_string()),
            ]
        );
        assert_eq!(
            causes(&document),
            vec![(1, "人"), (2, "未熟 %% 文字の一部"), (1, "title は要因")]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nishikawa\n    %% 先頭のメモ\n    結果\n        人\n            未熟 %% 文字の一部\n        title は要因\n        %% 途中のメモ"
        );
        assert_eq!(merman_fish(&emitted), merman_fish(source));
        assert_renders(&emitted);
    }

    #[test]
    fn unicode_whitespace_is_text_not_indentation() {
        // `merman` indents by spaces and tabs only, so the ideographic space
        // leaves `b` at column 0 — a main bone — and is trimmed off its text.
        let source = "ishikawa-beta\n  結果\n    a\n      a1\n\u{3000}\u{3000}b";
        let document = parse(source);
        assert_eq!(causes(&document), vec![(1, "a"), (2, "a1"), (1, "b")]);
        assert_eq!(model_fish(&document), merman_fish(source));
        let emitted = assert_stable(source);
        assert_eq!(merman_fish(&emitted), merman_fish(source));
    }

    #[test]
    fn constructs_that_would_change_meaning_are_refused() {
        for (source, expected) in [
            ("ishikawa-beta %% メモ\n    a", "コメント"),
            ("ishikawa-beta\n    結果\n        a %%{ b", "%%{"),
            ("ishikawa-beta 結果 %%{ x", "%%{"),
        ] {
            let error = parse_ishikawa(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }
    }

    #[test]
    fn an_empty_fence_and_a_bare_header_are_a_fish_without_a_head() {
        assert_eq!(parse(""), IshikawaDocument::empty());
        assert_eq!(parse("ishikawa-beta"), IshikawaDocument::empty());
        assert_eq!(emit_ishikawa(&IshikawaDocument::empty()), "ishikawa-beta");
        assert_eq!(merman_fish("ishikawa-beta"), (None, Vec::new()));
        let bare = parse("ISHIKAWA");
        assert_eq!(bare.keyword, "ISHIKAWA");
        assert_eq!(emit_ishikawa(&bare), "ISHIKAWA");
    }

    #[test]
    fn texts_that_cannot_be_written_are_reported() {
        for text in [
            "",
            " 前の空白",
            "後の空白 ",
            "改行\nあり",
            "%% コメント",
            "a %%{ b",
        ] {
            assert!(!is_valid_ishikawa_text(text), "{text:?}");
        }
        for text in ["日本語", "50%", "a %% b", "title x", "C# 言語"] {
            assert!(is_valid_ishikawa_text(text), "{text:?}");
            let document = IshikawaDocument {
                effect: Some("結果".to_string()),
                causes: vec![IshikawaCause::new(text)],
                ..IshikawaDocument::empty()
            };
            assert_eq!(document.structure_error(), None);
            let emitted = emit_ishikawa(&document);
            assert_eq!(parse(&emitted), document, "{emitted}");
            assert_eq!(merman_fish(&emitted), model_fish(&document), "{emitted}");
            assert_renders(&emitted);
        }

        let headless = IshikawaDocument {
            causes: vec![IshikawaCause::new("a")],
            ..IshikawaDocument::empty()
        };
        assert!(
            headless
                .structure_error()
                .is_some_and(|error| error.contains("結果"))
        );
        let empty_cause = IshikawaDocument {
            effect: Some("結果".to_string()),
            causes: vec![IshikawaCause::new(" ")],
            ..IshikawaDocument::empty()
        };
        assert!(
            empty_cause
                .structure_error()
                .is_some_and(|error| error.contains("空の要因"))
        );
        let bad_effect = IshikawaDocument {
            effect: Some("%% 結果".to_string()),
            ..IshikawaDocument::empty()
        };
        assert!(bad_effect.structure_error().is_some());
    }

    #[test]
    fn an_outline_that_is_not_a_tree_is_reported() {
        let cause = |depth: usize| IshikawaCause {
            depth,
            ..IshikawaCause::new("x")
        };
        for causes in [vec![cause(2)], vec![cause(1), cause(3)], vec![cause(0)]] {
            let document = IshikawaDocument {
                effect: Some("結果".to_string()),
                causes,
                ..IshikawaDocument::empty()
            };
            assert!(document.structure_error().is_some(), "{document:?}");
        }
    }

    #[test]
    fn outline_edits_move_whole_bones() {
        let mut entries = parse(TEMPLATE).causes;
        assert_eq!(outline_move(&mut entries, 4, false), Some(2));
        assert!(outline_indent(&mut entries, 4), "方法 goes under 材料");
        assert_eq!(
            outline_add_child(&mut entries, 0, IshikawaCause::new("疲労")),
            Some(2)
        );
        assert_eq!(
            outline_add_sibling(&mut entries, 0, IshikawaCause::new("環境")),
            Some(3)
        );
        assert_eq!(outline_outdent(&mut entries, 2), Some(2));
        let removed = outline_remove(&mut entries, 3).expect("a main bone can go");
        assert_eq!(removed.len(), 1);
        let document = IshikawaDocument {
            causes: entries,
            ..parse(TEMPLATE)
        };
        assert_eq!(document.structure_error(), None);
        let emitted = emit_ishikawa(&document);
        assert_eq!(
            emitted,
            "ishikawa-beta\n    %% 最初の行が結果、字下げした行が要因\n    不良品が多い\n        人\n            経験が浅い\n        疲労\n        材料\n            仕入れ先のばらつき\n            方法\n                手順書が古い"
        );
        assert_eq!(merman_fish(&emitted), model_fish(&document));
        assert_renders(&emitted);
    }

    #[test]
    fn a_deep_chain_is_read_without_recursion() {
        let mut source = String::from("ishikawa-beta\n  Root\n");
        for depth in 0..300 {
            source.push_str(&" ".repeat((depth + 2) * 2));
            source.push_str(&format!("Node {depth}\n"));
        }
        let document = parse(&source);
        assert_eq!(document.causes.len(), 300);
        assert_eq!(document.causes.last().map(|cause| cause.depth), Some(300));
        let emitted = emit_ishikawa(&document);
        assert_eq!(parse(&emitted), document);
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "ishikawa-beta  \r\n    結果  \r\n        a\r\n";
        let document = parse(source);
        assert_eq!(document.effect.as_deref(), Some("結果"));
        assert_eq!(causes(&document), vec![(1, "a")]);
        assert_eq!(
            emit_ishikawa(&document),
            "ishikawa-beta\n    結果\n        a"
        );
    }

    /// Mermaid cuts a directive out of its line and reads what follows as a
    /// cause, which a preserved comment would move below the fish.
    #[test]
    fn a_cause_after_a_directive_on_the_same_line_is_refused() {
        let source = "ishikawa-beta\n    結果\n        a\n%%{init: {}}%%         b\n        c";
        let (_, edges) = merman_fish(source);
        assert!(edges.contains(&("a".to_string(), "b".to_string())));
        let message = format!("{}", parse_ishikawa(source).err().expect("refused"));
        assert!(message.contains("指示子"), "{message}");
    }
}
