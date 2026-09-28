//! A round-trippable editing model for Mermaid tree views (`treeView-beta`).
//!
//! [`parse_tree_view`] reads the body of a ```` ```mermaid ```` fence into
//! [`TreeViewDocument`], and [`emit_tree_view`] writes it back. See
//! [`crate::mermaid_model`] for the invariants every diagram kind keeps.
//!
//! A tree view is a list of quoted names whose nesting is spelled with
//! indentation. `merman` reads it relatively, like a mindmap: a name hangs
//! under the last name above it that is indented less, and the names that hang
//! under nothing are siblings under an invisible root. The model keeps the tree
//! as an outline — each name with its depth, 1 for the top level — and forgets
//! the widths, so the emitter can write every level four spaces deeper than
//! its parent without changing which name hangs under which.
//!
//! What this module refuses rather than rewrites, all of it with the reason in
//! Japanese:
//!
//! - a `%%` comment at the end of a line, which `merman` drops;
//! - a second `title`, of which `merman` keeps only the last;
//! - `%%{` inside a line, which Mermaid's preprocessing reads as the start of a
//!   directive and cuts out together with everything up to the next `}%%`;
//! - anything else `merman` does not read as a tree view.
//!
//! `accTitle` and `accDescr` statements and a bare `title` are held verbatim as
//! preserved lines, which are written before the first name the way `merman`
//! requires. Comment lines between names move below the tree the way they do
//! in [`crate::mindmap_model`].

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};
use crate::mindmap_model::OutlineItem;

const HEADER: &str = "treeView-beta";

/// How deep `merman` lets a tree view nest before it refuses the diagram.
pub const MAX_TREE_VIEW_DEPTH: usize = merman::MAX_DIAGRAM_NESTING_DEPTH;

/// One name of the tree, in the order the fence writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeViewEntry {
    /// 1 for the top level, 2 for its children and so on.
    pub depth: usize,
    /// The drawn text, without the quotes around it.
    pub name: String,
    /// Whether the name is written between `'` rather than `"`. Either works;
    /// the one that was read is kept so that an unedited line is written back
    /// byte for byte.
    pub single_quoted: bool,
}

impl TreeViewEntry {
    /// A top-level name written between `"`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            depth: 1,
            name: name.into(),
            single_quoted: false,
        }
    }

    /// Switches to the other quote when the name contains the one it is
    /// written with and not the other. A name that fits keeps its quote.
    pub fn fit_quote(&mut self) {
        let (own, other) = if self.single_quoted {
            ('\'', '"')
        } else {
            ('"', '\'')
        };
        if self.name.contains(own) && !self.name.contains(other) {
            self.single_quoted = !self.single_quoted;
        }
    }

    fn quote(&self) -> char {
        if self.single_quoted { '\'' } else { '"' }
    }
}

impl OutlineItem for TreeViewEntry {
    fn depth(&self) -> usize {
        self.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }
}

/// An editable tree view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeViewDocument {
    /// The text of the `title` statement. A `title` with nothing after it is
    /// held as a preserved line instead, so `None` means "no title to edit".
    pub title: Option<String>,
    pub entries: Vec<TreeViewEntry>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl TreeViewDocument {
    /// A blank tree view, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            entries: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_tree_view_title(title)
        {
            return Some(format!(
                "タイトル `{title}` は書けません (改行と `%%` は使えず、空や前後の空白もできません)。"
            ));
        }
        let mut previous_depth = 0;
        for entry in &self.entries {
            if entry.depth == 0 || entry.depth > previous_depth + 1 {
                return Some(format!(
                    "ノード `{}` が親より 2 段以上深くなっています。",
                    entry.name
                ));
            }
            if entry.depth > MAX_TREE_VIEW_DEPTH {
                return Some(format!(
                    "ノード `{}` の階層が深すぎます (Mermaid は {MAX_TREE_VIEW_DEPTH} 段までです)。",
                    entry.name
                ));
            }
            if let Some(problem) = tree_view_name_problem(entry) {
                return Some(problem);
            }
            previous_depth = entry.depth;
        }
        None
    }
}

/// Whether `source` is a tree view: its first statement is `treeView-beta`.
pub fn is_tree_view_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_tree_view(source: &str) -> Result<TreeViewDocument> {
    let document = scan_tree_view(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model(), RenderSemanticModel::TreeView(_)) {
            bail!("この図はツリービューではありません。");
        }
    }

    Ok(document)
}

fn scan_tree_view(source: &str) -> Result<TreeViewDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(TreeViewDocument {
            title: None,
            entries: Vec::new(),
            preserved_lines,
        });
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, HEADER) else {
        bail!("ツリービューではありません (`treeView-beta` で始まる図だけを構造化編集できます)。");
    };
    if tail.starts_with("%%") {
        bail!(
            "`treeView-beta` の行末のコメントは構造化編集で失われます。コメントを独立した行に移すか、テキストのまま編集してください。"
        );
    }
    if !tail.is_empty() {
        bail!(
            "`treeView-beta` の行の `{tail}` は Mermaid が受け付けません。テキストのまま編集してください。"
        );
    }

    let mut title: Option<String> = None;
    let mut seen_title = false;
    // Each name with the indentation it was written at, which only says
    // something relative to the names around it.
    let mut written: Vec<(usize, String, bool)> = Vec::new();
    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
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
                    "指示子 `%%{{…}}%%` の後ろの `{rest}` は Mermaid がノードとして読みますが、構造化編集ではコメントと一緒に動いてしまいます。指示子を独立した行にしてください。"
                );
            }
            preserved_lines.push(PreservedLine { position, text });
            continue;
        }

        let statement = strip_inline_comment(line);
        if statement.trim_end() != line.trim_end() {
            bail!(
                "文の末尾のコメントは構造化編集で失われます ({trimmed})。コメントを独立した行に移すか、テキストのまま編集してください。"
            );
        }
        if trimmed.contains("%%{") {
            bail!(
                "`%%{{` を含む行は Mermaid が指示子として切り取るので、構造化編集に対応していません ({trimmed})。"
            );
        }

        // `merman` reads these only before the first name; after it they are
        // names without quotes, which it refuses.
        if written.is_empty() {
            if let Some(value) = read_title(statement) {
                if seen_title {
                    bail!(
                        "`title` が 2 回書かれています。Mermaid は後の方だけを使うので、1 つにまとめてからもう一度開いてください。"
                    );
                }
                seen_title = true;
                if value.is_empty() {
                    preserved_lines.push(PreservedLine {
                        position: PreservedPosition::Top,
                        text: line.trim_end().to_string(),
                    });
                } else {
                    title = Some(value);
                }
                continue;
            }
            if is_accessibility_statement(statement) {
                preserved_lines.push(PreservedLine {
                    position: PreservedPosition::Top,
                    text: line.trim_end().to_string(),
                });
                continue;
            }
        }

        let Some((indent, name, single_quoted)) = read_name_line(statement) else {
            bail!(
                "ツリービューのノードとして読めませんでした ({trimmed})。名前は \"…\" か '…' で囲み、`title` `accTitle` `accDescr` は最初のノードより前に書きます。"
            );
        };
        written.push((indent, name, single_quoted));
        position = PreservedPosition::Bottom;
    }

    // A name's parent is the last name above it that is indented less, which
    // is what the stack of open indentations holds at every step.
    let mut open_indents: Vec<usize> = Vec::new();
    let mut entries = Vec::with_capacity(written.len());
    for (indent, name, single_quoted) in written {
        while open_indents.last().is_some_and(|open| *open >= indent) {
            open_indents.pop();
        }
        let depth = open_indents.len() + 1;
        if depth > MAX_TREE_VIEW_DEPTH {
            bail!(
                "ノード `{name}` の階層が深すぎます (Mermaid は {MAX_TREE_VIEW_DEPTH} 段までです)。"
            );
        }
        entries.push(TreeViewEntry {
            depth,
            name,
            single_quoted,
        });
        open_indents.push(indent);
    }

    Ok(TreeViewDocument {
        title,
        entries,
        preserved_lines,
    })
}

/// The value of a `title` statement, read the way `merman`'s `parse_title`
/// reads it: `title` alone is an empty title.
fn read_title(statement: &str) -> Option<String> {
    let rest = statement.trim_start().strip_prefix("title")?;
    if rest.chars().all(char::is_whitespace) {
        return Some(String::new());
    }
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim().to_string())
}

/// Whether `merman` reads the statement as `accTitle: …`, `accDescr: …` or a
/// one-line `accDescr { … }`.
fn is_accessibility_statement(statement: &str) -> bool {
    let statement = statement.trim();
    if let Some(rest) = statement.strip_prefix("accTitle") {
        return rest.trim_start().starts_with(':');
    }
    let Some(rest) = statement.strip_prefix("accDescr") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with(':') || (rest.starts_with('{') && rest.ends_with('}'))
}

/// The indentation, the name and whether it is quoted with `'`, read the way
/// `merman`'s `parse_node_line` reads a line: only spaces and tabs count as
/// indentation, one column each.
fn read_name_line(statement: &str) -> Option<(usize, String, bool)> {
    let rest = statement.trim_start_matches([' ', '\t']);
    let indent = statement.len() - rest.len();
    let quote = rest
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))?;
    let body = rest.get(quote.len_utf8()..)?;
    let end = body.find(quote)?;
    let name = body.get(..end)?;
    let tail = body.get(end + quote.len_utf8()..)?;
    if !tail.trim().is_empty() {
        return None;
    }
    Some((indent, name.to_string(), quote == '\''))
}

/// Cuts `line` at the first `%%` outside a `"` or `'` string, the way
/// `merman`'s tree view parser does before it reads a line.
fn strip_inline_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut characters = line.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        match character {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '%' if !in_single && !in_double => {
                if characters.peek().is_some_and(|(_, next)| *next == '%') {
                    return line.get(..offset).unwrap_or_default();
                }
            }
            _ => {}
        }
    }
    line
}

fn emit_name_line(entry: &TreeViewEntry) -> String {
    let quote = entry.quote();
    format!("{}{quote}{}{quote}", INDENT.repeat(entry.depth), entry.name)
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_tree_view(document: &TreeViewDocument) -> String {
    let mut out = String::new();
    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(HEADER);
    out.push('\n');
    // A bare `title` is the empty title the title field replaces; writing both
    // would give the fence two titles, which the parser refuses.
    for line in preserved_at(document, PreservedPosition::Top)
        .filter(|line| document.title.is_none() || read_title(line).is_none())
    {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(title) = &document.title {
        out.push_str(&format!("{INDENT}title {title}\n"));
    }
    for entry in &document.entries {
        out.push_str(&emit_name_line(entry));
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
    document: &TreeViewDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether the entry's line reads back as the same name. A name can hold
/// anything but a line break, `%%{` and the quote it is written between; an
/// empty name is drawn as an empty row, which `merman` accepts.
pub fn is_valid_tree_view_name(entry: &TreeViewEntry) -> bool {
    if entry.name.contains(['\n', '\r'])
        || entry.name.contains("%%{")
        || rewritten_as_html_attribute(entry)
    {
        return false;
    }
    let line = emit_name_line(entry);
    strip_inline_comment(&line) == line
        && read_name_line(&line).is_some_and(|(_, name, single_quoted)| {
            name == entry.name && single_quoted == entry.single_quoted
        })
}

/// Whether `title` can be written after `title` and read back as itself.
pub fn is_valid_tree_view_title(title: &str) -> bool {
    if title.trim().is_empty() || title.contains(['\n', '\r']) || title.contains("%%{") {
        return false;
    }
    let line = format!("{INDENT}title {title}");
    strip_inline_comment(&line) == line && read_title(&line).as_deref() == Some(title)
}

/// Mermaid's preprocessing rewrites `="…"` inside an HTML tag to `='…'` before
/// the tree view parser runs, which ends a name written between `'` early.
fn rewritten_as_html_attribute(entry: &TreeViewEntry) -> bool {
    entry.single_quoted && entry.name.contains('<') && entry.name.contains("=\"")
}

/// What follows the last `}%%` of a directive block, when there is something.
/// Mermaid cuts the directive out and reads the rest as an ordinary line,
/// which a preserved comment block would carry to another place in the tree.
pub(crate) fn text_after_directive(block: &str) -> Option<&str> {
    if !block.trim_start().starts_with("%%{") {
        return None;
    }
    let end = block.rfind("}%%")?;
    let rest = block.get(end + "}%%".len()..)?.trim();
    (!rest.is_empty() && !rest.starts_with("%%")).then_some(rest)
}

fn tree_view_name_problem(entry: &TreeViewEntry) -> Option<String> {
    if entry.name.contains(['\n', '\r']) {
        return Some("ノードの名前に改行は使えません。".to_string());
    }
    if entry.name.contains("%%{") {
        return Some(format!(
            "ノード `{}` の名前に `%%{{` は使えません (Mermaid が指示子として切り取ります)。",
            entry.name
        ));
    }
    if rewritten_as_html_attribute(entry) {
        return Some(format!(
            "ノード `{}` の名前は書けません (HTML タグの属性を `\"` で囲むと、Mermaid が `'` に書き換えて名前が途中で切れます)。",
            entry.name
        ));
    }
    if !is_valid_tree_view_name(entry) {
        return Some(format!(
            "ノード `{}` の名前は書けません (`\"` と `'` を両方使うことはできません)。",
            entry.name
        ));
    }
    None
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

    const TEMPLATE: &str = "treeView-beta\n    %% 字下げで階層を表す\n    \"プロジェクト\"\n        \"src\"\n            \"main.rs\"\n            \"lib.rs\"\n        \"docs\"\n            \"README.md\"";

    /// Every example `merman` ships for tree views (its `fixtures/treeView`).
    const MERMAN_EXAMPLES: &[&str] = &[
        "treeView-beta\n            \"root\"\n                \"folder1\"\n                    \"file1.js\"\n                    \"file2.ts\"\n                \"folder2\"\n                    \"file3.spec.ts\"\n                    \"folder3\"\n                        \"file4.ts\"\n                        \"file5.ts\"\n                        \"folder4\"\n                            \"file6.ts\"\n                \"file7.ts\"\n",
        "treeView-beta\n            \"folder1\"\n                \"file1.js\"\n                \"file2.ts\"\n            \"folder2\"\n                \"file3.spec.ts\"\n                \"folder3\"\n                    \"file4.ts\"\n                    \"file5.ts\"\n                    \"folder4\"\n                        \"file6.ts\"\n            \"file7.ts\"\n",
        "treeView-beta\n            \"file1.ts\"\n",
        "---\nconfig:\n  treeView:\n      rowIndent: 80\n      lineThickness: 3\n  themeVariables:\n      treeView:\n          labelFontSize: '20px'\n          labelColor: '#FF0000'\n          lineColor: '#00FF00'\n---\ntreeView-beta\n      \"folder1\"\n          \"file1.js\"\n          \"file2.ts\"\n      \"folder2\"\n          \"file3.spec.ts\"\n          \"folder3\"\n              \"file4.ts\"\n              \"file5.ts\"\n              \"folder4\"\n                  \"file6.ts\"\n      \"file7.ts\"\n",
        "treeView-beta\n    \"packages\"\n        \"mermaid\"\n            \"src\"\n        \"parser\"\n",
        "treeView-beta\ntitle My TreeView Diagram\naccTitle: Accessible Title\naccDescr: This is an accessible description\n\"Root\"\n    \"Child\"\n",
    ];

    fn parse(source: &str) -> TreeViewDocument {
        parse_tree_view(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_tree_view(&first);
        let second = parse(&first_text);
        let second_text = emit_tree_view(&second);
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

    fn names(document: &TreeViewDocument) -> Vec<(usize, &str)> {
        document
            .entries
            .iter()
            .map(|entry| (entry.depth, entry.name.as_str()))
            .collect()
    }

    /// The title and each name with its parent's name (`/` for the invisible
    /// root), the way `merman` builds the tree.
    fn merman_tree(source: &str) -> (Option<String>, Vec<(String, String)>) {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source:?}: {error}"))
            .unwrap_or_else(|| panic!("merman should find a diagram in {source:?}"));
        let RenderSemanticModel::TreeView(model) = parsed.into_parts().1 else {
            panic!("{source:?} should be a tree view");
        };
        let mut edges = Vec::new();
        let mut pending = vec![&model.root];
        while let Some(node) = pending.pop() {
            for child in &node.children {
                edges.push((node.name.clone(), child.name.clone()));
                pending.push(child);
            }
        }
        edges.sort();
        (model.title, edges)
    }

    fn model_tree(document: &TreeViewDocument) -> (Option<String>, Vec<(String, String)>) {
        let entries = &document.entries;
        let mut edges = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let parent = outline_parent(entries, index)
                    .and_then(|parent| entries.get(parent))
                    .map_or("/".to_string(), |parent| parent.name.clone());
                (parent, entry.name.clone())
            })
            .collect::<Vec<_>>();
        edges.sort();
        (document.title.clone(), edges)
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        let source = "treeView-beta\n    \"src\"";
        assert!(
            is_tree_view_source(source),
            "{source} should be a tree view"
        );
        for source in [
            "treeView\n  \"src\"",
            "treeview-beta",
            "treeView-betas",
            "",
            "%% tree_view",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_tree_view_source(source),
                "{source} should not be a tree view"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(
            names(&document),
            vec![
                (1, "プロジェクト"),
                (2, "src"),
                (3, "main.rs"),
                (3, "lib.rs"),
                (2, "docs"),
                (3, "README.md"),
            ]
        );
        assert_eq!(document.structure_error(), None);
        assert_eq!(emit_tree_view(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_merman_example_round_trips_and_renders() {
        for source in MERMAN_EXAMPLES {
            let document = parse(source);
            assert_eq!(document.structure_error(), None, "{source}");
            assert_eq!(model_tree(&document), merman_tree(source), "{source}");
            let emitted = assert_stable(source);
            assert_eq!(merman_tree(&emitted), merman_tree(source), "{emitted}");
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_title_and_accessibility_lines_stay_before_the_names() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\ntreeView-beta\n    %% 先頭のメモ\n    accTitle: 概要\n    title 構成\n    accDescr { 説明 }\n    \"a\"\n    %% 途中のメモ\n        'b'";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("構成"));
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
                (PreservedPosition::Top, "accTitle: 概要".to_string()),
                (PreservedPosition::Top, "accDescr { 説明 }".to_string()),
                (PreservedPosition::Bottom, "%% 途中のメモ".to_string()),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\ntreeView-beta\n    %% 先頭のメモ\n    accTitle: 概要\n    accDescr { 説明 }\n    title 構成\n    \"a\"\n        'b'\n    %% 途中のメモ"
        );
        assert_eq!(merman_tree(&emitted), merman_tree(source));
        assert_renders(&emitted);
    }

    #[test]
    fn a_bare_title_is_kept_as_it_was() {
        let source = "treeView-beta\n    title\n    \"a\"";
        let document = parse(source);
        assert_eq!(document.title, None);
        assert_eq!(assert_stable(source), source);
    }

    #[test]
    fn the_depth_is_read_relatively_and_rewritten_in_four_space_steps() {
        // `merman` counts a tab as one column, so the tabbed `c` and `x` are
        // indented less than `r` and go back to the top level.
        let source = "treeView-beta\n  \"r\"\n        \"a\"\n    \"b\"\n\t\t\"c\"\n\t\"x\"";
        let document = parse(source);
        assert_eq!(model_tree(&document), merman_tree(source));
        assert_eq!(
            names(&document),
            vec![(1, "r"), (2, "a"), (2, "b"), (1, "c"), (1, "x")]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "treeView-beta\n    \"r\"\n        \"a\"\n        \"b\"\n    \"c\"\n    \"x\""
        );
        assert_eq!(merman_tree(&emitted), merman_tree(source));
    }

    #[test]
    fn quotes_percent_signs_and_multibyte_names_survive() {
        let source = "treeView-beta\n    \"it's\"\n        'say \"hi\"'\n        \"50%% 完了\"\n        \"\"\n        \"  空白つき  \"";
        let document = parse(source);
        assert_eq!(
            names(&document),
            vec![
                (1, "it's"),
                (2, "say \"hi\""),
                (2, "50%% 完了"),
                (2, ""),
                (2, "  空白つき  "),
            ]
        );
        assert_eq!(document.structure_error(), None);
        assert_eq!(assert_stable(source), source);
        assert_eq!(merman_tree(source), model_tree(&document));
        assert_renders(source);
    }

    #[test]
    fn constructs_that_would_be_dropped_or_misread_are_refused() {
        for (source, expected) in [
            ("treeView-beta\n    \"a\" %% メモ", "コメント"),
            ("treeView-beta %% メモ\n    \"a\"", "行末のコメント"),
            ("treeView-beta\n    title x %% メモ\n    \"a\"", "コメント"),
            ("treeView-beta\n    title a\n    title b", "2 回"),
            (
                "treeView-beta\n    \"a\"\n    title b",
                "ノードとして読めません",
            ),
            ("treeView-beta\n    a", "ノードとして読めません"),
            ("treeView-beta\n    \"a\" b", "ノードとして読めません"),
            ("treeView-beta\n    \"a", "ノードとして読めません"),
            ("treeView-beta\n    \"a %%{ b\"", "%%{"),
            ("treeView-beta LR\n    \"a\"", "受け付けません"),
            (
                "treeView-beta\n    accDescr {\n    説明\n    }",
                "ノードとして読めません",
            ),
        ] {
            let error = parse_tree_view(source)
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
    fn too_deep_a_tree_is_refused_like_merman_refuses_it() {
        let chain = |depth: usize| {
            let mut source = String::from("treeView-beta\n");
            for level in 0..depth {
                source.push_str(&" ".repeat(level));
                source.push_str(&format!("\"n{level}\"\n"));
            }
            source
        };
        let deepest = chain(MAX_TREE_VIEW_DEPTH);
        let document = parse(&deepest);
        assert_eq!(document.structure_error(), None);
        let emitted = emit_tree_view(&document);
        assert_eq!(parse(&emitted), document);

        let too_deep = chain(MAX_TREE_VIEW_DEPTH + 1);
        assert!(
            Engine::new()
                .parse_diagram_for_render_model_sync(&too_deep, ParseOptions::strict())
                .is_err(),
            "merman should refuse the chain too"
        );
        let message = format!("{}", parse_tree_view(&too_deep).err().expect("refused"));
        assert!(message.contains("深すぎます"), "{message}");

        let mut edited = document;
        edited.entries.push(TreeViewEntry {
            depth: MAX_TREE_VIEW_DEPTH + 1,
            ..TreeViewEntry::new("x")
        });
        assert!(
            edited
                .structure_error()
                .is_some_and(|error| error.contains("深すぎます"))
        );
    }

    #[test]
    fn an_empty_fence_and_a_bare_header_are_a_tree_without_names() {
        assert_eq!(parse(""), TreeViewDocument::empty());
        assert_eq!(parse("treeView-beta"), TreeViewDocument::empty());
        assert_eq!(emit_tree_view(&TreeViewDocument::empty()), "treeView-beta");
    }

    #[test]
    fn names_and_titles_that_cannot_be_written_are_reported() {
        let mut both = TreeViewEntry::new("\" と ' の両方");
        both.fit_quote();
        assert!(!is_valid_tree_view_name(&both));
        for name in ["改行\nあり", "a %%{ b"] {
            assert!(
                !is_valid_tree_view_name(&TreeViewEntry::new(name)),
                "{name}"
            );
        }

        let mut double = TreeViewEntry::new("say \"hi\"");
        assert!(!is_valid_tree_view_name(&double));
        double.fit_quote();
        assert!(double.single_quoted);
        assert!(is_valid_tree_view_name(&double));
        assert_eq!(emit_name_line(&double), "    'say \"hi\"'");

        // A name that fits keeps the quote it was read with.
        let mut single = TreeViewEntry {
            single_quoted: true,
            ..TreeViewEntry::new("plain")
        };
        single.fit_quote();
        assert!(single.single_quoted);

        for title in ["", " 前の空白", "改行\nあり", "50 %% x", "a %%{ b"] {
            assert!(!is_valid_tree_view_title(title), "{title:?}");
        }
        for title in ["構成", "it's 50%%"] {
            assert!(is_valid_tree_view_title(title), "{title:?}");
        }

        let document = TreeViewDocument {
            title: Some("構成".to_string()),
            entries: vec![TreeViewEntry::new("a"), both],
            preserved_lines: Vec::new(),
        };
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("両方"))
        );
    }

    #[test]
    fn an_outline_that_is_not_a_tree_is_reported() {
        let entry = |depth: usize| TreeViewEntry {
            depth,
            ..TreeViewEntry::new("x")
        };
        for entries in [vec![entry(2)], vec![entry(1), entry(3)], vec![entry(0)]] {
            let document = TreeViewDocument {
                entries,
                ..TreeViewDocument::empty()
            };
            assert!(document.structure_error().is_some(), "{document:?}");
        }
    }

    const TREE: &str = "treeView-beta\n    \"a\"\n        \"a1\"\n        \"a2\"\n    \"b\"\n        \"b1\"\n    \"c\"";

    fn emit_entries(entries: Vec<TreeViewEntry>) -> String {
        let document = TreeViewDocument {
            entries,
            ..TreeViewDocument::empty()
        };
        assert_eq!(document.structure_error(), None);
        emit_tree_view(&document)
    }

    /// The top level holds many names, so unlike a mindmap's root every one of
    /// them can be moved, removed and given siblings.
    #[test]
    fn outline_edits_work_on_top_level_names_too() {
        let mut entries = parse(TREE).entries;
        assert_eq!(outline_move(&mut entries, 0, true), Some(2));
        assert_eq!(
            emit_entries(entries.clone()),
            "treeView-beta\n    \"b\"\n        \"b1\"\n    \"a\"\n        \"a1\"\n        \"a2\"\n    \"c\""
        );
        assert_eq!(outline_move(&mut entries, 2, false), Some(0));
        assert_eq!(emit_entries(entries.clone()), TREE);

        assert!(outline_indent(&mut entries, 3), "b goes under a");
        assert_eq!(
            emit_entries(entries.clone()),
            "treeView-beta\n    \"a\"\n        \"a1\"\n        \"a2\"\n        \"b\"\n            \"b1\"\n    \"c\""
        );
        assert_eq!(outline_outdent(&mut entries, 3), Some(3));
        assert_eq!(emit_entries(entries.clone()), TREE);
        assert_eq!(outline_outdent(&mut entries, 0), None);

        let removed = outline_remove(&mut entries, 0).expect("a top-level name can go");
        assert_eq!(removed.len(), 3);
        assert_eq!(
            emit_entries(entries.clone()),
            "treeView-beta\n    \"b\"\n        \"b1\"\n    \"c\""
        );
        assert_eq!(
            outline_add_sibling(&mut entries, 0, TreeViewEntry::new("新")),
            Some(2)
        );
        assert_eq!(
            outline_add_child(&mut entries, 3, TreeViewEntry::new("子")),
            Some(4)
        );
        let emitted = emit_entries(entries);
        assert_eq!(
            emitted,
            "treeView-beta\n    \"b\"\n        \"b1\"\n    \"新\"\n    \"c\"\n        \"子\""
        );
        assert_renders(&emitted);
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "treeView-beta  \r\n    \"a\"  \r\n        \"b\"\r\n";
        let document = parse(source);
        assert_eq!(names(&document), vec![(1, "a"), (2, "b")]);
        assert_eq!(
            emit_tree_view(&document),
            "treeView-beta\n    \"a\"\n        \"b\""
        );
    }

    /// A bare `title` stands for the empty title, so a title typed into the
    /// field takes its place instead of being written as a second `title`,
    /// which the parser would refuse when the fence is opened again.
    #[test]
    fn a_typed_title_replaces_a_bare_one() {
        let mut document =
            parse("treeView-beta\n    %% メモ\n    title\n    accTitle: 概要\n    \"a\"");
        document.title = Some("構成".to_string());
        let emitted = emit_tree_view(&document);
        assert_eq!(
            emitted,
            "treeView-beta\n    %% メモ\n    accTitle: 概要\n    title 構成\n    \"a\""
        );
        assert_eq!(parse(&emitted).title.as_deref(), Some("構成"));
        assert_eq!(merman_tree(&emitted).0.as_deref(), Some("構成"));
    }

    /// Mermaid rewrites `="…"` inside an HTML tag to `='…'` before the tree
    /// view parser runs, so such a name cannot be written between `'`.
    #[test]
    fn a_name_with_a_double_quoted_html_attribute_is_reported() {
        let mut entry = TreeViewEntry::new("<b class=\"x\">太字</b>");
        entry.fit_quote();
        assert!(entry.single_quoted);
        assert!(!is_valid_tree_view_name(&entry));
        let document = TreeViewDocument {
            entries: vec![entry],
            ..TreeViewDocument::empty()
        };
        let emitted = emit_tree_view(&document);
        assert!(
            Engine::new()
                .parse_diagram_for_render_model_sync(&emitted, ParseOptions::strict())
                .is_err(),
            "merman should refuse {emitted}, or this check is no longer needed"
        );
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("HTML"))
        );

        for name in ["<b class='x'>太字</b>", "a=\"b\"", "<b>太字</b>"] {
            let mut entry = TreeViewEntry::new(name);
            entry.fit_quote();
            assert!(is_valid_tree_view_name(&entry), "{name}");
            let emitted = emit_tree_view(&TreeViewDocument {
                entries: vec![entry],
                ..TreeViewDocument::empty()
            });
            assert_eq!(names(&parse(&emitted)), vec![(1, name)], "{emitted}");
            assert_renders(&emitted);
        }
    }

    /// Mermaid cuts a directive out of its line and reads what follows as a
    /// name, which a preserved comment would move below the tree.
    #[test]
    fn a_name_after_a_directive_on_the_same_line_is_refused() {
        let source = "treeView-beta\n    \"a\"\n%%{init: {}}%%     \"b\"\n    \"c\"";
        let (_, edges) = merman_tree(source);
        assert!(edges.contains(&("a".to_string(), "b".to_string())));
        let message = format!("{}", parse_tree_view(source).err().expect("refused"));
        assert!(message.contains("指示子"), "{message}");

        let multi_line = "treeView-beta\n    \"a\"\n%%{init: {\n}}%% \"b\"";
        assert!(parse_tree_view(multi_line).is_err());

        // A comment after the directive is dropped by Mermaid as well.
        let commented = "treeView-beta\n    \"a\"\n%%{init: {}}%% %% メモ";
        assert_eq!(
            assert_stable(commented),
            "treeView-beta\n    \"a\"\n%%{init: {}}%% %% メモ"
        );
    }
}
