//! The entry point of the structured Mermaid editors: which kind of diagram a
//! fence holds, and the document types the editor edits.
//!
//! Every diagram kind has a module of its own ([`crate::flowchart_model`],
//! [`crate::pie_model`], [`crate::state_model`], [`crate::sequence_model`],
//! [`crate::er_model`], [`crate::class_model`], [`crate::gantt_model`]) with the
//! same three functions — `is_<kind>_source`, `parse_<kind>` and `emit_<kind>` —
//! and they all keep the same two promises:
//!
//! - Emitting is idempotent from its second application, so opening and
//!   confirming the editor twice does not keep rewriting the fence.
//! - Nothing disappears. A construct the model does not carry is either held
//!   verbatim in a preserved line, or the parse fails with a reason in
//!   Japanese, which the editor shows instead of silently dropping the
//!   construct.
//!
//! The scanning helpers that all the kinds need live here, because the parts of
//! a fence that come before the diagram keyword — YAML frontmatter, `%%{init}%%`
//! directives and comments — look the same whatever the diagram is.

use anyhow::{Result, bail};

use crate::class_model::{ClassDocument, emit_class, is_class_source, parse_class};
use crate::er_model::{ErDocument, emit_er, is_er_source, parse_er};
use crate::flowchart_model::{
    FlowchartDocument, PreservedLine, PreservedPosition, emit_flowchart, is_flowchart_source,
    parse_flowchart,
};
use crate::gantt_model::{GanttDocument, emit_gantt, is_gantt_source, parse_gantt};
use crate::gitgraph_model::{GitGraphDocument, emit_gitgraph, is_gitgraph_source, parse_gitgraph};
use crate::mindmap_model::{MindmapDocument, emit_mindmap, is_mindmap_source, parse_mindmap};
use crate::pie_model::{PieDocument, emit_pie, is_pie_source, parse_pie};
use crate::sequence_model::{SequenceDocument, emit_sequence, is_sequence_source, parse_sequence};
use crate::state_model::{StateDocument, emit_state, is_state_source, parse_state};
use crate::timeline_model::{TimelineDocument, emit_timeline, is_timeline_source, parse_timeline};

/// The indentation the emitters write statements with. Mermaid ignores it; it
/// is here so that a fence the editor wrote looks like one a person wrote.
pub(crate) const INDENT: &str = "    ";

/// The diagram kinds the structured editor knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidDiagramKind {
    Flowchart,
    Pie,
    State,
    Sequence,
    Er,
    Class,
    Gantt,
    Mindmap,
    Timeline,
    GitGraph,
}

impl MermaidDiagramKind {
    /// Every kind, in the order the editor's menus list them.
    pub const ALL: [MermaidDiagramKind; 10] = [
        MermaidDiagramKind::Flowchart,
        MermaidDiagramKind::Pie,
        MermaidDiagramKind::State,
        MermaidDiagramKind::Sequence,
        MermaidDiagramKind::Er,
        MermaidDiagramKind::Class,
        MermaidDiagramKind::Gantt,
        MermaidDiagramKind::Mindmap,
        MermaidDiagramKind::Timeline,
        MermaidDiagramKind::GitGraph,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Flowchart => "フローチャート",
            Self::Pie => "円グラフ",
            Self::State => "状態遷移図",
            Self::Sequence => "シーケンス図",
            Self::Er => "ER 図",
            Self::Class => "クラス図",
            Self::Gantt => "ガントチャート",
            Self::Mindmap => "マインドマップ",
            Self::Timeline => "タイムライン",
            Self::GitGraph => "Git グラフ",
        }
    }
}

/// An editable Mermaid diagram.
#[derive(Debug, Clone, PartialEq)]
pub enum MermaidDocument {
    Flowchart(FlowchartDocument),
    Pie(PieDocument),
    State(StateDocument),
    Sequence(SequenceDocument),
    Er(ErDocument),
    Class(ClassDocument),
    Gantt(GanttDocument),
    Mindmap(MindmapDocument),
    Timeline(TimelineDocument),
    GitGraph(GitGraphDocument),
}

impl MermaidDocument {
    /// A blank document of `kind`, used when the editor is opened on an empty
    /// fence.
    pub fn empty(kind: MermaidDiagramKind) -> Self {
        match kind {
            MermaidDiagramKind::Flowchart => Self::Flowchart(FlowchartDocument::empty()),
            MermaidDiagramKind::Pie => Self::Pie(PieDocument::empty()),
            MermaidDiagramKind::State => Self::State(StateDocument::empty()),
            MermaidDiagramKind::Sequence => Self::Sequence(SequenceDocument::empty()),
            MermaidDiagramKind::Er => Self::Er(ErDocument::empty()),
            MermaidDiagramKind::Class => Self::Class(ClassDocument::empty()),
            MermaidDiagramKind::Gantt => Self::Gantt(GanttDocument::empty()),
            MermaidDiagramKind::Mindmap => Self::Mindmap(MindmapDocument::empty()),
            MermaidDiagramKind::Timeline => Self::Timeline(TimelineDocument::empty()),
            MermaidDiagramKind::GitGraph => Self::GitGraph(GitGraphDocument::empty()),
        }
    }

    pub fn kind(&self) -> MermaidDiagramKind {
        match self {
            Self::Flowchart(_) => MermaidDiagramKind::Flowchart,
            Self::Pie(_) => MermaidDiagramKind::Pie,
            Self::State(_) => MermaidDiagramKind::State,
            Self::Sequence(_) => MermaidDiagramKind::Sequence,
            Self::Er(_) => MermaidDiagramKind::Er,
            Self::Class(_) => MermaidDiagramKind::Class,
            Self::Gantt(_) => MermaidDiagramKind::Gantt,
            Self::Mindmap(_) => MermaidDiagramKind::Mindmap,
            Self::Timeline(_) => MermaidDiagramKind::Timeline,
            Self::GitGraph(_) => MermaidDiagramKind::GitGraph,
        }
    }

    /// Writes the document back as Mermaid source, without a trailing newline.
    pub fn emit(&self) -> String {
        match self {
            Self::Flowchart(document) => emit_flowchart(document),
            Self::Pie(document) => emit_pie(document),
            Self::State(document) => emit_state(document),
            Self::Sequence(document) => emit_sequence(document),
            Self::Er(document) => emit_er(document),
            Self::Class(document) => emit_class(document),
            Self::Gantt(document) => emit_gantt(document),
            Self::Mindmap(document) => emit_mindmap(document),
            Self::Timeline(document) => emit_timeline(document),
            Self::GitGraph(document) => emit_gitgraph(document),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    ///
    /// Only the kinds whose items refer to each other across the whole
    /// document — a message names a participant, a relationship names an
    /// entity, a task starts after another task — can be brought into a state
    /// that emitting would spoil; the others are checked field by field by the
    /// editor.
    pub fn structure_error(&self) -> Option<String> {
        match self {
            Self::Sequence(document) => document.structure_error(),
            Self::Er(document) => document.structure_error(),
            Self::Gantt(document) => document.structure_error(),
            Self::Mindmap(document) => document.structure_error(),
            Self::Timeline(document) => document.structure_error(),
            Self::GitGraph(document) => document.structure_error(),
            Self::Flowchart(_) | Self::Pie(_) | Self::State(_) | Self::Class(_) => None,
        }
    }
}

/// Which kind of diagram `source` is, judged by its first statement, or `None`
/// when it is a kind the structured editor does not know — including an empty
/// fence, for which the caller picks the kind itself.
///
/// Cheap enough to call before parsing.
pub fn detect_kind(source: &str) -> Option<MermaidDiagramKind> {
    if is_flowchart_source(source) {
        return Some(MermaidDiagramKind::Flowchart);
    }
    if is_pie_source(source) {
        return Some(MermaidDiagramKind::Pie);
    }
    if is_state_source(source) {
        return Some(MermaidDiagramKind::State);
    }
    if is_sequence_source(source) {
        return Some(MermaidDiagramKind::Sequence);
    }
    if is_er_source(source) {
        return Some(MermaidDiagramKind::Er);
    }
    if is_class_source(source) {
        return Some(MermaidDiagramKind::Class);
    }
    if is_gantt_source(source) {
        return Some(MermaidDiagramKind::Gantt);
    }
    if is_mindmap_source(source) {
        return Some(MermaidDiagramKind::Mindmap);
    }
    if is_timeline_source(source) {
        return Some(MermaidDiagramKind::Timeline);
    }
    if is_gitgraph_source(source) {
        return Some(MermaidDiagramKind::GitGraph);
    }
    None
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_document(source: &str) -> Result<MermaidDocument> {
    match detect_kind(source) {
        Some(MermaidDiagramKind::Flowchart) => {
            parse_flowchart(source).map(MermaidDocument::Flowchart)
        }
        Some(MermaidDiagramKind::Pie) => parse_pie(source).map(MermaidDocument::Pie),
        Some(MermaidDiagramKind::State) => parse_state(source).map(MermaidDocument::State),
        Some(MermaidDiagramKind::Sequence) => parse_sequence(source).map(MermaidDocument::Sequence),
        Some(MermaidDiagramKind::Er) => parse_er(source).map(MermaidDocument::Er),
        Some(MermaidDiagramKind::Class) => parse_class(source).map(MermaidDocument::Class),
        Some(MermaidDiagramKind::Gantt) => parse_gantt(source).map(MermaidDocument::Gantt),
        Some(MermaidDiagramKind::Mindmap) => parse_mindmap(source).map(MermaidDocument::Mindmap),
        Some(MermaidDiagramKind::Timeline) => parse_timeline(source).map(MermaidDocument::Timeline),
        Some(MermaidDiagramKind::GitGraph) => parse_gitgraph(source).map(MermaidDocument::GitGraph),
        None => {
            bail!("この図は構造化エディターに対応していません。テキストのまま編集してください。")
        }
    }
}

/// The first line that says which diagram this is: the first non-empty line
/// that is not YAML frontmatter, a `%%{init}%%` directive or a comment.
pub(crate) fn first_statement_line(source: &str) -> Option<&str> {
    let mut lines = source.lines().map(str::trim).peekable();
    let mut in_frontmatter = false;
    let mut seen_content = false;
    let mut in_directive = false;

    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if in_directive {
            if line.contains("}%%") {
                in_directive = false;
            }
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if !seen_content && line == "---" {
            seen_content = true;
            in_frontmatter = true;
            continue;
        }
        seen_content = true;
        if line.starts_with("%%") {
            if line.starts_with("%%{") && !line.contains("}%%") {
                in_directive = true;
            }
            continue;
        }
        return Some(line);
    }
    None
}

/// What the leading lines of a fence hold: everything before the diagram
/// keyword, and where the keyword is.
pub(crate) struct Preamble {
    pub preserved: Vec<PreservedLine>,
    /// Index into the scanned lines of the diagram keyword, or `None` for a
    /// fence that holds nothing but frontmatter, directives and comments.
    pub header: Option<usize>,
}

/// Reads the frontmatter, `%%{init}%%` directives and comments that come before
/// the diagram keyword. They have to stay in front of it, so they are all
/// preserved at [`PreservedPosition::Preamble`].
pub(crate) fn scan_preamble(lines: &[&str]) -> Preamble {
    let mut preserved = Vec::new();
    let mut index = 0;
    let mut seen_content = false;

    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        let start = index;
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        if !seen_content && trimmed == "---" {
            seen_content = true;
            index += 1;
            while lines.get(index).is_some_and(|line| line.trim() != "---") {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        seen_content = true;
        if trimmed.starts_with("%%") {
            index += 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        return Preamble {
            preserved,
            header: Some(index),
        };
    }

    Preamble {
        preserved,
        header: None,
    }
}

/// What follows `keyword` at the start of `line`, with its leading whitespace
/// removed, or `None` when the line does not start with that word.
///
/// The word has to end where Mermaid's lexers end it: at whitespace, at the
/// statement separator or at the end of the line. Without that check `pieces`
/// would read as a `pie` chart.
pub(crate) fn keyword_tail<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if rest.is_empty() || rest.starts_with(|character: char| character.is_whitespace()) {
        return Some(rest.trim_start());
    }
    None
}

/// [`keyword_tail`] for the keywords whose lexer ignores case and lets the word
/// end at a delimiter as well — `accDescr{`, `classDef:` and their like.
pub(crate) fn statement_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let prefix = line.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = line.get(keyword.len()..)?;
    if rest.is_empty()
        || rest.starts_with(|character: char| {
            character.is_whitespace()
                || matches!(character, ':' | '{' | '}' | '[' | ']' | '"' | ';')
        })
    {
        return Some(rest.trim_start());
    }
    None
}

/// Joins source lines into one preserved entry, keeping their original
/// indentation and dropping only the whitespace at their ends.
pub(crate) fn join_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_japanese_label() {
        for kind in MermaidDiagramKind::ALL {
            let label = kind.label();
            assert!(
                label.chars().any(|character| character > '\u{7f}'),
                "{kind:?} should have a Japanese label, got {label}"
            );
        }
    }

    #[test]
    fn the_kind_is_detected_through_frontmatter_and_comments() {
        for (source, expected) in [
            (
                "flowchart TD\n  A --> B",
                Some(MermaidDiagramKind::Flowchart),
            ),
            ("graph LR\n  A --> B", Some(MermaidDiagramKind::Flowchart)),
            ("pie title x\n  \"a\" : 1", Some(MermaidDiagramKind::Pie)),
            (
                "stateDiagram-v2\n  [*] --> A",
                Some(MermaidDiagramKind::State),
            ),
            ("stateDiagram\n  [*] --> A", Some(MermaidDiagramKind::State)),
            (
                "sequenceDiagram\n  A->>B: hi",
                Some(MermaidDiagramKind::Sequence),
            ),
            ("erDiagram\n  A ||--o{ B : x", Some(MermaidDiagramKind::Er)),
            ("classDiagram\n  class A", Some(MermaidDiagramKind::Class)),
            (
                "gantt\n  A :a, 2026-01-01, 7d",
                Some(MermaidDiagramKind::Gantt),
            ),
            (
                "---\ntitle: t\n---\n%%{init: {\"theme\": \"dark\"}}%%\n%% メモ\npie\n  \"a\" : 1",
                Some(MermaidDiagramKind::Pie),
            ),
            ("", None),
            ("%% メモだけ", None),
            ("mindmap\n  root((中心))", Some(MermaidDiagramKind::Mindmap)),
            (
                "timeline\n    2024 : 企画",
                Some(MermaidDiagramKind::Timeline),
            ),
            ("journey\n  title x", None),
        ] {
            assert_eq!(detect_kind(source), expected, "for {source}");
        }
    }

    #[test]
    fn a_document_round_trips_through_the_common_entry_point() {
        for source in [
            "flowchart TD\n    A[開始] --> B[終了]",
            "pie title 内訳\n    \"設計\" : 30\n    \"実装\" : 70",
            "stateDiagram-v2\n    [*] --> Idle\n    Idle --> [*]",
            "sequenceDiagram\n    participant A\n    participant B\n    A->>B: 依頼",
            "erDiagram\n    CUSTOMER {\n        string name\n    }\n    CUSTOMER ||--o{ ORDER : places",
            "classDiagram\n    class 動物 {\n        +鳴く() void\n    }\n    動物 <|-- 犬",
            "gantt\n    dateFormat YYYY-MM-DD\n    section 設計\n    要件定義 :req, 2026-01-06, 5d",
            "mindmap\n  root((中心))\n    枝[四角]\n    ::icon(fa fa-book)",
            "timeline\n    title 沿革\n    section 前半\n    2024 : 企画 : 設計",
        ] {
            let document = parse_document(source)
                .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
            let emitted = document.emit();
            let again = parse_document(&emitted)
                .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{emitted}"));
            assert_eq!(document.kind(), again.kind());
            assert_eq!(document, again, "round trip of {source} gave {emitted}");
            assert_eq!(emitted, again.emit());
        }
    }

    #[test]
    fn a_blank_document_is_available_for_every_kind() {
        for kind in MermaidDiagramKind::ALL {
            let document = MermaidDocument::empty(kind);
            assert_eq!(document.kind(), kind);
            assert_eq!(document.structure_error(), None, "{kind:?}");
            let emitted = document.emit();
            assert_eq!(
                detect_kind(&emitted),
                Some(kind),
                "a blank {kind:?} should emit source of its own kind, got {emitted}"
            );
            assert_eq!(
                parse_document(&emitted)
                    .unwrap_or_else(|error| panic!("a blank {kind:?} should parse: {error}")),
                document,
                "a blank {kind:?} should survive a round trip"
            );
        }
    }

    #[test]
    fn a_kind_with_no_model_at_all_is_refused() {
        for source in ["quadrantChart\n  title x", "journey\n  title x"] {
            let error = parse_document(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(message.contains("構造化エディター"), "got: {message}");
        }
    }

    #[test]
    fn a_keyword_has_to_end_where_the_lexer_ends_it() {
        assert_eq!(keyword_tail("pie showData", "pie"), Some("showData"));
        assert_eq!(keyword_tail("pie", "pie"), Some(""));
        assert_eq!(keyword_tail("pieces", "pie"), None);
        assert_eq!(keyword_tail("日本語", "pie"), None);

        assert_eq!(statement_keyword("accDescr {", "accDescr"), Some("{"));
        assert_eq!(statement_keyword("accdescr: x", "accDescr"), Some(": x"));
        assert_eq!(statement_keyword("accDescription", "accDescr"), None);
        assert_eq!(statement_keyword("あ", "accDescr"), None);
    }
}
