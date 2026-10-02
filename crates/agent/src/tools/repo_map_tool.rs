use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use fs::Fs;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use language::{Buffer, BufferSnapshot, LanguageRegistry, OutlineItem, Point, Rope};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;
use util::paths::PathStyle;
use util::rel_path::{RelPath, RelPathBuf};

/// Returns a compact map of the project: the non-ignored files, each followed by
/// the signatures of the symbols it declares (functions, types, traits, impl
/// blocks, ...) with their line numbers.
///
/// Use this to learn the overall structure of a repository before reading
/// individual files — it is far cheaper than reading files, because it only
/// contains declarations, never bodies. Follow it up with `read_file` (using the
/// reported line numbers) or `grep` once you know where to look.
///
/// Symbols come from the tree-sitter outline of each file, so files in languages
/// without an outline grammar, files that are too large, and binary files are
/// listed by path only.
///
/// The result is truncated to fit a byte budget: first the functions of the
/// later files lose their parameters, then files are listed by path only. When
/// it is truncated, narrow it down with `path` rather than raising `max_bytes`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RepoMapToolInput {
    /// Optional path prefix to restrict the map to. Like other tools, the first
    /// component must be a project root directory.
    ///
    /// <example>
    /// If the project has a root directory `my-app`, pass `my-app/src/models`
    /// to map only that subtree. Omit this field to map the whole project.
    /// </example>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum size of the map in bytes. Defaults to 32768 (roughly 8k tokens).
    /// Values are clamped to the range 1024..=262144.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

pub struct RepoMapTool {
    project: Entity<Project>,
}

impl RepoMapTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

const DEFAULT_MAX_BYTES: usize = 32_768;
const MIN_MAX_BYTES: usize = 1_024;
const HARD_MAX_BYTES: usize = 262_144;

/// Files above this size are listed by path only: they are usually generated or
/// vendored, and parsing them would spend the whole budget on one entry.
const MAX_FILE_SIZE: u64 = 512 * 1024;

/// Upper bound on how many files are parsed, so the tool stays bounded on very
/// large repositories even when few of them yield symbols.
const MAX_FILES_PARSED: usize = 1_000;

const MAX_SYMBOLS_PER_FILE: usize = 200;

/// Longest signature kept before its tail is elided.
const MAX_SIGNATURE_CHARS: usize = 200;

/// A declaration whose header does not end within this many rows keeps its
/// outline text: what follows is more likely to be a body than parameters.
/// Wrapped parameter lists of twenty-odd rows do occur in this codebase.
const MAX_SIGNATURE_ROWS: u32 = 32;

/// Cap on the source one signature may consume, so that a minified file cannot
/// make a single scan proportional to its line length.
const MAX_SIGNATURE_SCAN_CHARS: usize = 4_096;

/// Keywords introducing a callable in the languages whose outline queries this
/// fork ships. Queries that instead capture the parentheses (Go, JavaScript,
/// TypeScript, C) are recognized by the empty parameter list they leave in the
/// outline text.
const FUNCTION_KEYWORDS: [&str; 4] = ["fn", "def", "func", "function"];

/// Fraction of the budget held back for the path-only listing and the footer.
const RESERVED_BUDGET_DIVISOR: usize = 5;

/// Space kept free for the trailing summary line.
const FOOTER_RESERVE: usize = 256;

const HEADER: &str = "# Repository map\n\
                      Each file is followed by its declarations, indented by nesting depth, with [Lstart-end] line numbers.\n\n";

struct Candidate {
    display_path: String,
    abs_path: PathBuf,
    depth: usize,
    size: u64,
    has_language: bool,
}

impl RepoMapTool {
    /// Also used to build the optional `<repo_map>` section of the system
    /// prompt (`agent.system_prompt_repo_map_bytes`, TODO F13 ③), so that the
    /// prompt and the tool can never drift apart.
    pub async fn build_map(
        project: Entity<Project>,
        input: RepoMapToolInput,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let max_bytes = input
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(MIN_MAX_BYTES, HARD_MAX_BYTES);
        let path_prefix = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty() && *path != "." && *path != "./");

        let (snapshots, language_registry, fs) = project.read_with(cx, |project, cx| {
            (
                project
                    .worktrees(cx)
                    .map(|worktree| worktree.read(cx).snapshot())
                    .collect::<Vec<_>>(),
                project.languages().clone(),
                project.fs().clone(),
            )
        });

        let path_prefix = path_prefix.map(str::to_owned);
        let candidates =
            cx.background_executor()
                .spawn({
                    let language_registry = language_registry.clone();
                    async move {
                        collect_candidates(&snapshots, path_prefix.as_deref(), &language_registry)
                    }
                })
                .await;

        if candidates.is_empty() {
            return Ok(match input.path.as_deref() {
                Some(path) => format!("No files found under {path}."),
                None => "The project has no files to map.".to_owned(),
            });
        }

        let total_files = candidates.len();
        let expanded_budget = max_bytes - max_bytes / RESERVED_BUDGET_DIVISOR;

        let mut output = String::with_capacity(max_bytes.min(1 << 16));
        output.push_str(HEADER);

        let mut parsed_files = 0;
        let mut budget_exhausted = false;
        let mut unexpanded = Vec::new();
        let mut blocks = Vec::new();
        // Admission is decided on the compact form, so that spelling out
        // parameters never costs a file its place in the map.
        let mut compact_length = output.len();

        for candidate in &candidates {
            if budget_exhausted
                || !candidate.has_language
                || candidate.size > MAX_FILE_SIZE
                || parsed_files >= MAX_FILES_PARSED
                || compact_length >= expanded_budget
            {
                unexpanded.push(candidate.display_path.as_str());
                continue;
            }

            parsed_files += 1;
            let block = match file_block(candidate, &language_registry, fs.as_ref(), cx).await {
                Some(block) => block,
                None => {
                    unexpanded.push(candidate.display_path.as_str());
                    continue;
                }
            };

            if compact_length + block.compact.len() > expanded_budget {
                budget_exhausted = true;
                unexpanded.push(candidate.display_path.as_str());
                continue;
            }

            compact_length += block.compact.len();
            blocks.push(block);
        }

        let expanded_files = blocks.len();
        let files_with_parameters =
            files_keeping_parameters(&blocks, expanded_budget.saturating_sub(compact_length));
        for (index, block) in blocks.iter().enumerate() {
            if index < files_with_parameters {
                output.push_str(&block.detailed);
            } else {
                output.push_str(&block.compact);
            }
        }
        let files_without_parameters = expanded_files - files_with_parameters;

        let mut listed_paths = 0;
        if !unexpanded.is_empty() {
            let listing_budget = max_bytes.saturating_sub(FOOTER_RESERVE);
            output.push_str("\n# Files without expanded symbols\n");
            for path in &unexpanded {
                if output.len() + path.len() + 1 > listing_budget {
                    break;
                }
                output.push_str(path);
                output.push('\n');
                listed_paths += 1;
            }
        }

        let omitted = unexpanded.len() - listed_paths;
        output.push_str(&format!(
            "\n{expanded_files} of {total_files} files expanded with symbols"
        ));
        if files_without_parameters > 0 {
            output.push_str(&format!(
                " (the last {files_without_parameters} without parameters)"
            ));
        }
        output.push_str(&format!(", {listed_paths} listed by path only"));
        if omitted > 0 {
            output.push_str(&format!(
                ", {omitted} omitted to stay within the byte budget"
            ));
        }
        output.push_str(".\n");

        Ok(output)
    }
}

/// How many of the leading blocks can show their parameters within `spare`
/// bytes on top of the compact map. Upgrading stops at the first block that
/// does not fit, so parameters survive for the trunk of the tree and the map
/// can say where they stop.
fn files_keeping_parameters(blocks: &[FileBlock], mut spare: usize) -> usize {
    let mut count = 0;
    for block in blocks {
        let extra = block.detailed.len().saturating_sub(block.compact.len());
        if extra > spare {
            break;
        }
        spare -= extra;
        count += 1;
    }
    count
}

fn collect_candidates(
    snapshots: &[worktree::Snapshot],
    path_prefix: Option<&str>,
    language_registry: &Arc<LanguageRegistry>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for snapshot in snapshots {
        let path_style = snapshot.path_style();
        let prefix = match path_prefix {
            Some(path_prefix) => match parse_prefix(path_prefix, path_style) {
                Some(prefix) => Some(prefix),
                // The prefix names a different worktree (or is not a relative
                // path at all), so this one contributes nothing.
                None => continue,
            },
            None => None,
        };

        for entry in snapshot.files(false, 0) {
            if entry.is_private || entry.is_external {
                continue;
            }
            let full_path = snapshot.root_name().join(&entry.path);
            if let Some(prefix) = &prefix
                && !full_path.starts_with(prefix)
            {
                continue;
            }
            candidates.push(Candidate {
                display_path: full_path.display(path_style).into_owned(),
                abs_path: snapshot.absolutize(&entry.path),
                depth: full_path.components().count(),
                size: entry.size,
                has_language: language_registry
                    .language_for_file_path(entry.path.as_std_path())
                    .is_some(),
            });
        }
    }

    // Shallow files first so the trunk of the tree survives truncation; the
    // remaining keys only make the order deterministic.
    candidates.sort_by(|left, right| {
        right
            .has_language
            .cmp(&left.has_language)
            .then_with(|| left.depth.cmp(&right.depth))
            .then_with(|| left.display_path.cmp(&right.display_path))
    });
    candidates
}

fn parse_prefix(path_prefix: &str, path_style: PathStyle) -> Option<RelPathBuf> {
    RelPath::new(Path::new(path_prefix), path_style)
        .ok()
        .map(|prefix| prefix.into_owned())
}

async fn file_block(
    candidate: &Candidate,
    language_registry: &Arc<LanguageRegistry>,
    fs: &dyn Fs,
    cx: &mut AsyncApp,
) -> Option<FileBlock> {
    let language = language_registry
        .load_language_for_file_path(&candidate.abs_path)
        .await
        .ok()?;
    language.grammar()?.outline_config.as_ref()?;

    // Non-UTF-8 files fail to load, which is how binaries are filtered out.
    let text = fs.load(&candidate.abs_path).await.ok()?;
    let snapshot = cx
        .update(|cx| {
            Buffer::build_snapshot(
                Rope::from(text),
                Some(language),
                Some(language_registry.clone()),
                None,
                cx,
            )
        })
        .await;

    let items = snapshot.outline_items_as_points_containing(0..snapshot.len(), true, None);
    if items.is_empty() {
        return None;
    }

    let mut block = FileBlock {
        detailed: String::new(),
        compact: String::new(),
    };
    block.push_both(&candidate.display_path);
    block.push_both("\n");
    for item in items.iter().take(MAX_SYMBOLS_PER_FILE) {
        if item.text.is_empty() {
            continue;
        }
        block.push_both("  ");
        for _ in 0..item.depth {
            block.push_both(" ");
        }
        match signature_for_item(&snapshot, item) {
            Some(signature) => {
                block.detailed.push_str(&signature);
                block.compact.push_str(&item.text);
            }
            None => block.push_both(&item.text),
        }
        let start_row = item.range.start.row + 1;
        let end_row = item.range.end.row + 1;
        if start_row == end_row {
            block.push_both(&format!(" [L{start_row}]\n"));
        } else {
            block.push_both(&format!(" [L{start_row}-{end_row}]\n"));
        }
    }
    if items.len() > MAX_SYMBOLS_PER_FILE {
        block.push_both(&format!(
            "  … {} more symbols\n",
            items.len() - MAX_SYMBOLS_PER_FILE
        ));
    }
    block.push_both("\n");
    Some(block)
}

/// A file's entry in the map, in the two forms the budget chooses between:
/// callables spelled out with their parameters, or named by the outline text.
struct FileBlock {
    detailed: String,
    compact: String,
}

impl FileBlock {
    fn push_both(&mut self, text: &str) {
        self.detailed.push_str(text);
        self.compact.push_str(text);
    }
}

/// How a declaration header is separated from the body it introduces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BodyDelimiter {
    /// `{` opens the body; a declaration that has no body ends at `;`.
    Brace,
    /// The header ends at the `:` that precedes an indented body.
    Colon,
    /// Unknown language: the header ends with the item's first line, or at a
    /// `{` or `;` on it if one comes first.
    LineEnd,
}

fn body_delimiter(language_name: Option<&str>) -> BodyDelimiter {
    match language_name {
        Some(
            "C" | "C#" | "C++" | "Dart" | "Go" | "Java" | "JavaScript" | "Kotlin" | "PHP" | "Rust"
            | "Scala" | "Swift" | "TSX" | "TypeScript" | "Zig",
        ) => BodyDelimiter::Brace,
        Some("Python") => BodyDelimiter::Colon,
        _ => BodyDelimiter::LineEnd,
    }
}

/// The outline text of a callable either carries the language's keyword for
/// one, or — when the outline query captures the parentheses instead — already
/// shows the parameter list emptied out, as in `function b( )`.
fn declares_parameters(outline_text: &str) -> bool {
    outline_text.ends_with("()")
        || outline_text.ends_with("( )")
        || outline_text
            .split_whitespace()
            .any(|token| FUNCTION_KEYWORDS.contains(&token))
}

/// Rebuilds a callable's declaration from the source, so that the map shows its
/// parameters instead of the bare `fn drop` the outline query produces.
///
/// The outline text cannot be used for this: `@context.extra` captures would
/// also reach the outline panel, and the outline builder truncates a multi-line
/// capture at its first row.
fn signature_for_item(snapshot: &BufferSnapshot, item: &OutlineItem<Point>) -> Option<String> {
    if !declares_parameters(&item.text) {
        return None;
    }

    let language_name = snapshot
        .language_at(item.range.start)
        .map(|language| language.name());
    let delimiter = body_delimiter(language_name.as_ref().map(|name| name.0.as_ref()));

    // C++ member declarations put the item on the declarator alone, leaving
    // the return type — part of the outline text — in front of the item, and
    // the terminating `;` after it.
    let item_is_declarator = item.source_range_for_text.start < item.range.start;
    let start = item.range.start.min(item.source_range_for_text.start);
    let window_end = item
        .range
        .end
        .min(Point::new(start.row + MAX_SIGNATURE_ROWS, 0))
        .min(snapshot.max_point());
    if window_end <= start {
        return None;
    }
    let header_ends_with_window = item_is_declarator && window_end == item.range.end;

    collapse_declaration_header(
        snapshot
            .text_for_range(start..window_end)
            .flat_map(|chunk| chunk.chars()),
        delimiter,
        header_ends_with_window,
    )
}

/// Collects the collapsed signature, keeping the spacing a reader expects
/// around brackets rather than the source's line breaks and indentation.
struct SignatureWriter {
    text: String,
    pending_separator: bool,
}

impl SignatureWriter {
    fn new() -> Self {
        Self {
            text: String::new(),
            pending_separator: false,
        }
    }

    fn separate(&mut self) {
        if !self.text.is_empty() {
            self.pending_separator = true;
        }
    }

    fn push(&mut self, character: char) {
        let after_open_bracket = matches!(self.text.chars().last(), Some('(') | Some('['));
        let hugs_previous = matches!(character, ')' | ']' | ',' | ';');
        if self.pending_separator && !after_open_bracket && !hugs_previous {
            self.text.push(' ');
        }
        self.pending_separator = false;
        // A trailing comma is an artifact of the source being wrapped over
        // several lines; the collapsed form reads as a single-line declaration.
        if matches!(character, ')' | ']') && self.text.ends_with(',') {
            self.text.pop();
        }
        self.text.push(character);
    }

    /// String literals keep their spacing, but not their line breaks: the map
    /// gives every symbol exactly one line.
    fn push_verbatim(&mut self, character: char) {
        self.pending_separator = false;
        self.text.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
}

/// Folds the declaration header the characters start with onto one line.
///
/// The characters are consumed lazily and only until the header ends, so the
/// window a caller offers can cover a whole wrapped parameter list without the
/// scan paying for the body that follows it.
///
/// Returns `None` when the header does not end within them, or when it ends
/// with brackets or a string still open — a half-captured parameter list would
/// read as a broken signature, so the caller keeps the outline text instead.
fn collapse_declaration_header(
    characters: impl Iterator<Item = char>,
    delimiter: BodyDelimiter,
    header_ends_with_characters: bool,
) -> Option<String> {
    let scanned = std::cell::Cell::new(0_usize);
    let mut characters = characters
        .inspect(|_| scanned.set(scanned.get() + 1))
        .take(MAX_SIGNATURE_SCAN_CHARS)
        .peekable();
    let mut writer = SignatureWriter::new();
    let mut depth: usize = 0;
    let mut string_delimiter: Option<char> = None;
    let mut escaped = false;
    let mut parameters_closed = false;
    let mut terminated = false;

    while let Some(character) = characters.next() {
        if let Some(open) = string_delimiter {
            writer.push_verbatim(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == open {
                string_delimiter = None;
            }
            continue;
        }

        let comment_start = match delimiter {
            BodyDelimiter::Brace => {
                character == '/' && matches!(characters.peek(), Some('/') | Some('*'))
            }
            BodyDelimiter::Colon => character == '#',
            BodyDelimiter::LineEnd => false,
        };
        if comment_start {
            if characters.peek() == Some(&'*') {
                characters.next();
                if !skip_block_comment(&mut characters) {
                    return None;
                }
            } else {
                while characters.peek().is_some_and(|peeked| *peeked != '\n') {
                    characters.next();
                }
            }
            writer.separate();
            continue;
        }

        match character {
            '"' | '`' => {
                writer.push(character);
                string_delimiter = Some(character);
            }
            '\'' if delimiter == BodyDelimiter::Colon => {
                writer.push(character);
                string_delimiter = Some(character);
            }
            '{' | ';' if depth == 0 && delimiter != BodyDelimiter::Colon => {
                terminated = true;
                break;
            }
            ':' if depth == 0 && delimiter == BodyDelimiter::Colon => {
                terminated = true;
                break;
            }
            '(' | '[' | '{' => {
                depth += 1;
                writer.push(character);
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                writer.push(character);
                if depth == 0 && character == ')' {
                    parameters_closed = true;
                }
            }
            '\n' => {
                let line_ends_header = delimiter == BodyDelimiter::LineEnd
                    || (delimiter == BodyDelimiter::Brace && depth == 0 && parameters_closed);
                if line_ends_header {
                    terminated = true;
                    break;
                }
                writer.separate();
            }
            character if character.is_whitespace() => writer.separate(),
            character => writer.push(character),
        }
    }

    if !terminated
        && header_ends_with_characters
        && parameters_closed
        && scanned.get() < MAX_SIGNATURE_SCAN_CHARS
    {
        terminated = true;
    }
    if !terminated || depth != 0 || string_delimiter.is_some() {
        return None;
    }

    let mut signature = writer.text;
    if !signature.contains('(') || !angle_brackets_are_balanced(&signature) {
        return None;
    }
    if signature.chars().count() > MAX_SIGNATURE_CHARS {
        let cut = signature
            .char_indices()
            .nth(MAX_SIGNATURE_CHARS - 1)
            .map(|(offset, _)| offset)?;
        signature.truncate(cut);
        signature.push('…');
    }
    Some(signature)
}

/// A generic parameter list left half-open reads as badly as an unclosed
/// parenthesis, but unlike brackets it cannot be tracked while scanning, since
/// `->` and `=>` carry a `>` of their own. Counting once the header is folded
/// is enough to notice one that was cut in half by a line break.
fn angle_brackets_are_balanced(signature: &str) -> bool {
    let mut depth: i32 = 0;
    let mut previous = ' ';
    for character in signature.chars() {
        match character {
            '<' => depth += 1,
            '>' if previous != '-' && previous != '=' => depth -= 1,
            _ => {}
        }
        previous = character;
    }
    depth == 0
}

/// Consumes the rest of a block comment whose `/*` was just read, reporting
/// whether its `*/` was reached.
fn skip_block_comment(characters: &mut std::iter::Peekable<impl Iterator<Item = char>>) -> bool {
    while let Some(character) = characters.next() {
        if character == '*' && characters.peek() == Some(&'/') {
            characters.next();
            return true;
        }
    }
    false
}

impl AgentTool for RepoMapTool {
    type Input = RepoMapToolInput;
    type Output = String;

    const NAME: &'static str = "repo_map";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input.ok().and_then(|input| input.path) {
            Some(path) => {
                let path = MarkdownInlineCode(&path);
                i18n::t!("Build a repository map of {path}").into()
            }
            None => i18n::t!("Build a repository map").into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            Self::build_map(project, input, cx)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn run_tool(
        project: Entity<Project>,
        input: RepoMapToolInput,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let tool = Arc::new(RepoMapTool::new(project));
        cx.update(|cx| {
            tool.run(
                ToolInput::resolved(input),
                ToolCallEventStream::test().0,
                cx,
            )
        })
        .await
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".gitignore": "ignored/\n",
                "README.md": "# hello",
                "src": {
                    "main.rs": "fn main() {}\n",
                    "lib.rs": "pub struct Config {}\n",
                },
                "ignored": {
                    "generated.rs": "fn generated() {}\n",
                },
                "notes.txt": "plain text",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();
        project
    }

    #[gpui::test]
    async fn test_repo_map_lists_files_and_respects_gitignore(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        assert!(output.starts_with("# Repository map"), "{output}");
        assert!(output.contains("main.rs"), "{output}");
        assert!(output.contains("notes.txt"), "{output}");
        // `ignored/` is gitignored, so the worktree snapshot never offers it.
        assert!(!output.contains("generated.rs"), "{output}");
        assert!(output.contains("files expanded with symbols"), "{output}");
    }

    #[gpui::test]
    async fn test_repo_map_path_prefix_filters(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: Some("project/src".into()),
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        assert!(output.contains("main.rs"), "{output}");
        assert!(output.contains("lib.rs"), "{output}");
        assert!(!output.contains("README.md"), "{output}");
        assert!(!output.contains("notes.txt"), "{output}");
    }

    #[gpui::test]
    async fn test_repo_map_unknown_prefix_is_reported(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: Some("project/nonexistent".into()),
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        assert_eq!(output, "No files found under project/nonexistent.");
    }

    #[gpui::test]
    async fn test_repo_map_honors_byte_budget(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let mut tree = serde_json::Map::new();
        for index in 0..200 {
            tree.insert(
                format!("file_{index:03}.rs"),
                json!(format!("pub fn function_{index:03}() {{}}\n")),
            );
        }
        fs.insert_tree(path!("/project"), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.update(|cx| project.read(cx).languages().add(language::rust_lang()));
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: Some(2_048),
            },
            cx,
        )
        .await
        .unwrap();

        assert!(output.len() <= 2_048, "map was {} bytes", output.len());
        assert!(output.contains("fn function_000"), "{output}");
        assert!(
            output.contains("of 200 files expanded with symbols"),
            "{output}"
        );
        assert!(
            output.contains("omitted to stay within the byte budget"),
            "{output}"
        );
        assert!(
            !output.contains("200 of 200 files expanded"),
            "budget should have truncated the map: {output}"
        );
    }

    #[gpui::test]
    async fn test_repo_map_includes_symbol_signatures(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "lib.rs": concat!(
                    "pub struct Config {\n",
                    "    pub name: String,\n",
                    "}\n",
                    "\n",
                    "impl Config {\n",
                    "    pub fn new() -> Self {\n",
                    "        Self { name: String::new() }\n",
                    "    }\n",
                    "}\n",
                ),
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.update(|cx| project.read(cx).languages().add(language::rust_lang()));
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        assert!(output.contains("struct Config"), "{output}");
        assert!(output.contains("impl Config"), "{output}");
        assert!(output.contains("pub fn new() -> Self"), "{output}");
        assert!(output.contains("[L"), "{output}");
        assert!(
            output.contains("1 of 1 files expanded with symbols"),
            "{output}"
        );
    }

    #[gpui::test]
    async fn test_repo_map_expands_multiline_signatures(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "lib.rs": concat!(
                    "pub struct Config {\n",
                    "    pub name: String,\n",
                    "}\n",
                    "\n",
                    "impl Config {\n",
                    "    #[inline]\n",
                    "    pub fn merge(\n",
                    "        &mut self,\n",
                    "        other: &Config, // wins on conflict\n",
                    "        strict: bool,\n",
                    "    ) -> Result<(), String> {\n",
                    "        Ok(())\n",
                    "    }\n",
                    "}\n",
                    "\n",
                    "impl Drop for Config {\n",
                    "    fn drop(&mut self) {}\n",
                    "}\n",
                ),
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.update(|cx| project.read(cx).languages().add(language::rust_lang()));
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        assert!(
            output.contains(
                "pub fn merge(&mut self, other: &Config, strict: bool) -> Result<(), String> [L7-13]"
            ),
            "{output}"
        );
        assert!(output.contains("fn drop(&mut self) [L17]"), "{output}");
        // Types and fields keep their outline text: they have no parameters to
        // spell out, and their bodies are what the map deliberately omits.
        assert!(output.contains("struct Config [L1-3]"), "{output}");
        assert!(output.contains("pub name [L2]"), "{output}");
        assert!(!output.contains("pub struct Config {"), "{output}");
    }

    #[test]
    fn only_callables_are_given_a_signature() {
        assert!(declares_parameters("pub fn new"));
        assert!(declares_parameters("async def load"));
        assert!(declares_parameters("func Serve"));
        // Go, JavaScript, TypeScript and C capture the parentheses instead of a
        // keyword, so their outline text already shows an emptied-out list.
        assert!(declares_parameters("function b( )"));
        assert!(declares_parameters("int parse()"));

        assert!(!declares_parameters("struct Config"));
        assert!(!declares_parameters("impl Config"));
        assert!(!declares_parameters("mod tools"));
        assert!(!declares_parameters("pub name"));
    }

    #[test]
    fn a_multiline_rust_signature_is_folded_onto_one_line() {
        let source = concat!(
            "pub fn merge(\n",
            "    &mut self,\n",
            "    other: &Config, /* wins */\n",
            "    strict: bool,\n",
            ") -> Result<(), String> {\n",
            "    Ok(())\n",
            "}\n",
        );
        assert_eq!(
            collapse_declaration_header(source.chars(), BodyDelimiter::Brace, false).as_deref(),
            Some("pub fn merge(&mut self, other: &Config, strict: bool) -> Result<(), String>")
        );
    }

    #[test]
    fn a_rust_declaration_without_a_body_ends_at_its_semicolon() {
        assert_eq!(
            collapse_declaration_header(
                "fn drop(&mut self);\n".chars(),
                BodyDelimiter::Brace,
                false
            )
            .as_deref(),
            Some("fn drop(&mut self)")
        );
    }

    #[test]
    fn a_python_definition_ends_at_its_colon() {
        let source = concat!(
            "def load(\n",
            "    path: Path,\n",
            "    *,\n",
            "    encoding: str = 'utf-8',  # ignored for binary files\n",
            ") -> dict[str, int]:\n",
            "    return {}\n",
        );
        assert_eq!(
            collapse_declaration_header(source.chars(), BodyDelimiter::Colon, false).as_deref(),
            Some("def load(path: Path, *, encoding: str = 'utf-8') -> dict[str, int]")
        );
    }

    #[test]
    fn a_python_default_holding_braces_does_not_end_the_header() {
        assert_eq!(
            collapse_declaration_header(
                "def load(options={'a': 1}):\n".chars(),
                BodyDelimiter::Colon,
                false
            )
            .as_deref(),
            Some("def load(options={'a': 1})")
        );
    }

    #[test]
    fn a_string_default_spanning_lines_stays_on_one_line() {
        let signature = collapse_declaration_header(
            "def render(template=\"\"\"\nhello\n\"\"\"):\n".chars(),
            BodyDelimiter::Colon,
            false,
        )
        .expect("the header ends at the colon");
        assert!(!signature.contains('\n'), "{signature}");
        assert_eq!(signature, "def render(template=\"\"\" hello \"\"\")");
    }

    #[test]
    fn an_unknown_language_stops_at_the_end_of_the_line() {
        assert_eq!(
            body_delimiter(Some("Ruby")),
            BodyDelimiter::LineEnd,
            "the fork ships no Ruby outline query, so it must fall back"
        );
        assert_eq!(
            collapse_declaration_header(
                "def load(path)\n  File.read(path)\nend\n".chars(),
                BodyDelimiter::LineEnd,
                false
            )
            .as_deref(),
            Some("def load(path)")
        );
    }

    #[test]
    fn a_header_left_unbalanced_falls_back_to_the_outline_text() {
        // A declaration wrapped over several lines in a language whose body
        // delimiter is unknown would read as a broken `def load(`.
        assert_eq!(
            collapse_declaration_header(
                "def load(\n  path\n)\n".chars(),
                BodyDelimiter::LineEnd,
                false
            ),
            None
        );
        // Nor does a header that never ends within the scanned window.
        assert_eq!(
            collapse_declaration_header(
                "pub fn merge(\n  &mut self,\n".chars(),
                BodyDelimiter::Brace,
                false
            ),
            None
        );
        // A wrapped generic list closes its parameters on the first line, which
        // would otherwise end the header at `fn map<F: Fn(&str) -> bool,`.
        assert_eq!(
            collapse_declaration_header(
                "fn map<\n    F: Fn(&str) -> bool,\n>(\n    predicate: F,\n) {\n".chars(),
                BodyDelimiter::Brace,
                false
            ),
            None
        );
        assert!(angle_brackets_are_balanced(
            "fn parse(source: &str) -> Result<Vec<Item>, Error>"
        ));
        assert!(!angle_brackets_are_balanced("fn map<F: Fn(&str) -> bool,"));
    }

    #[test]
    fn a_long_signature_is_elided() {
        let mut source = String::from("pub fn wide(");
        for index in 0..40 {
            source.push_str(&format!("argument_{index}: u32, "));
        }
        source.push_str(") {}\n");

        let signature = collapse_declaration_header(source.chars(), BodyDelimiter::Brace, false)
            .expect("the header ends at the body brace");
        assert_eq!(signature.chars().count(), MAX_SIGNATURE_CHARS);
        assert!(signature.ends_with('…'), "{signature}");
        assert!(
            signature.starts_with("pub fn wide(argument_0: u32, argument_1: u32,"),
            "{signature}"
        );
    }

    fn bundled_language(name: &str) -> Arc<language::Language> {
        let grammar = grammars::native_grammars()
            .into_iter()
            .find_map(|(grammar_name, grammar)| (grammar_name == name).then_some(grammar))
            .unwrap_or_else(|| panic!("no bundled grammar named {name}"));
        Arc::new(
            language::Language::new(grammars::load_config(name), Some(grammar))
                .with_queries(grammars::load_queries(name))
                .unwrap_or_else(|error| panic!("queries for {name} do not parse: {error}")),
        )
    }

    #[gpui::test]
    async fn test_repo_map_spells_out_parameters_in_every_outlined_language(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "store.ts": concat!(
                    "export class Store {\n",
                    "  async load(path: string, retries = 3): Promise<void> {\n",
                    "  }\n",
                    "}\n",
                    "\n",
                    "export function merge<T>(\n",
                    "  left: T,\n",
                    "  right: Partial<T>,\n",
                    "): T {\n",
                    "  return left;\n",
                    "}\n",
                ),
                "loader.py": concat!(
                    "class Loader:\n",
                    "    @staticmethod\n",
                    "    async def load(path: str, *, encoding: str = \"utf-8\") -> bytes:\n",
                    "        return b\"\"\n",
                ),
                "server.go": concat!(
                    "package main\n",
                    "\n",
                    "type Server struct {\n",
                    "\taddr string\n",
                    "}\n",
                    "\n",
                    "func (s *Server) Serve(ctx context.Context, port int) error {\n",
                    "\treturn nil\n",
                    "}\n",
                    "\n",
                    "func New(addr string) *Server {\n",
                    "\treturn &Server{addr: addr}\n",
                    "}\n",
                ),
                "parse.c": concat!(
                    "struct point {\n",
                    "    int x;\n",
                    "};\n",
                    "\n",
                    "static int parse(const char *source, size_t length)\n",
                    "{\n",
                    "    return 0;\n",
                    "}\n",
                    "\n",
                    "int count(void);\n",
                ),
                "widget.cpp": concat!(
                    "class Widget {\n",
                    "public:\n",
                    "    Widget(int width, int height);\n",
                    "    int area() const;\n",
                    "    virtual void draw(Canvas &canvas) = 0;\n",
                    "};\n",
                    "\n",
                    "int Widget::area() const {\n",
                    "    return 0;\n",
                    "}\n",
                ),
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.update(|cx| {
            let languages = project.read(cx).languages();
            for name in ["typescript", "python", "go", "c", "cpp"] {
                languages.add(bundled_language(name));
            }
        });
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        for expected in [
            "async load(path: string, retries = 3): Promise<void> [L2-3]",
            "function merge<T>(left: T, right: Partial<T>): T [L6-11]",
            "async def load(path: str, *, encoding: str = \"utf-8\") -> bytes [L3-4]",
            "func (s *Server) Serve(ctx context.Context, port int) error [L7-9]",
            "func New(addr string) *Server [L11-13]",
            "static int parse(const char *source, size_t length) [L5-8]",
            "int count(void) [L10]",
            "Widget(int width, int height) [L3]",
            // Member declarations put the outline item on the declarator, so
            // the return type in front of it and the `;` after it lie outside.
            "int area() const [L4]",
            "void draw(Canvas &canvas) [L5]",
            "int Widget::area() const [L8-10]",
        ] {
            // Whole lines, so that a header scan starting too early (an
            // `export`, a class name) cannot hide behind a substring match.
            assert!(
                output.lines().any(|line| line.trim_start() == expected),
                "missing {expected:?} in:\n{output}"
            );
        }
        assert!(
            output.contains("5 of 5 files expanded with symbols, 0 listed"),
            "{output}"
        );
    }

    #[gpui::test]
    async fn test_repo_map_drops_parameters_before_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let mut tree = serde_json::Map::new();
        for file in 0..10 {
            let mut source = String::new();
            for function in 0..3 {
                source.push_str(&format!(
                    "pub fn function_{file}_{function}(first_argument: u32, second_argument: u32) -> u32 {{ 0 }}\n"
                ));
            }
            tree.insert(format!("file_{file}.rs"), json!(source));
        }
        fs.insert_tree(path!("/project"), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.update(|cx| project.read(cx).languages().add(language::rust_lang()));
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: Some(2_048),
            },
            cx,
        )
        .await
        .unwrap();

        assert!(output.len() <= 2_048, "map was {} bytes", output.len());
        // Every file fits once its functions are named without parameters,
        // which the full signatures (about 2.5 KB here) would not allow.
        assert!(
            output.contains("10 of 10 files expanded with symbols (the last "),
            "{output}"
        );
        assert!(
            output.contains(
                "pub fn function_0_0(first_argument: u32, second_argument: u32) -> u32 [L1]"
            ),
            "the trunk keeps its parameters: {output}"
        );
        assert!(
            output.contains("  pub fn function_9_2 [L3]"),
            "the last file is named only: {output}"
        );
        let first_compact_file = (0..10)
            .find(|file| output.contains(&format!("  pub fn function_{file}_0 [L1]")))
            .expect("some file is compacted");
        assert!(first_compact_file > 0, "{output}");
        for file in first_compact_file..10 {
            assert!(
                !output.contains(&format!("pub fn function_{file}_0(")),
                "parameters stop at a single point: {output}"
            );
        }
        assert!(
            output.contains(&format!(
                "(the last {} without parameters)",
                10 - first_compact_file
            )),
            "{output}"
        );
    }

    /// "The last K" names a suffix of the map: files without parameters to drop
    /// that come after the first compacted file are part of it, or the model
    /// would take the compacted files before them for complete signatures.
    #[gpui::test]
    async fn test_repo_map_counts_the_whole_compacted_tail(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let mut tree = serde_json::Map::new();
        for file in 0..10 {
            let mut source = String::new();
            for function in 0..3 {
                source.push_str(&format!(
                    "pub fn function_{file}_{function}(first_argument: u32, second_argument: u32) -> u32 {{ 0 }}\n"
                ));
            }
            tree.insert(format!("file_{file}.rs"), json!(source));
        }
        // Sorted after the files with functions, so they sit in the compacted tail.
        for file in 0..3 {
            tree.insert(
                format!("types_{file}.rs"),
                json!(format!("pub struct Type{file};\n")),
            );
        }
        fs.insert_tree(path!("/project"), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.update(|cx| project.read(cx).languages().add(language::rust_lang()));
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: Some(2_400),
            },
            cx,
        )
        .await
        .unwrap();

        assert!(
            output.contains("13 of 13 files expanded with symbols (the last "),
            "{output}"
        );
        for file in 0..3 {
            assert!(output.contains(&format!("struct Type{file}")), "{output}");
        }
        let compacted_function_files = (0..10)
            .filter(|file| output.contains(&format!("  pub fn function_{file}_0 [L1]")))
            .count();
        assert!((1..10).contains(&compacted_function_files), "{output}");
        for file in 0..10 {
            let compacted = output.contains(&format!("  pub fn function_{file}_0 [L1]"));
            assert_eq!(
                compacted,
                file >= 10 - compacted_function_files,
                "parameters stop at one file and stay dropped after it: {output}"
            );
        }
        let tail = compacted_function_files + 3;
        assert!(
            output.contains(&format!("(the last {tail} without parameters)")),
            "{output}"
        );
    }

    #[test]
    fn a_member_declarator_ends_where_its_item_ends() {
        assert_eq!(
            collapse_declaration_header("int area() const".chars(), BodyDelimiter::Brace, true)
                .as_deref(),
            Some("int area() const")
        );
        // Anywhere else, running out of characters means the header was cut.
        assert_eq!(
            collapse_declaration_header("int area() const".chars(), BodyDelimiter::Brace, false),
            None
        );
    }

    #[gpui::test]
    async fn test_repo_map_skips_private_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".env": "SECRET=1",
                "main.rs": "fn main() {}\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();

        assert!(output.contains("main.rs"), "{output}");
        assert!(!output.contains(".env"), "{output}");
    }
}
