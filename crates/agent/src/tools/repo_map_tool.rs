use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use fs::Fs;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use language::{Buffer, LanguageRegistry, Rope};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
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
/// The result is truncated to fit a byte budget. When it is truncated, narrow it
/// down with `path` rather than raising `max_bytes`.
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
    async fn build_map(
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

        let mut expanded_files = 0;
        let mut parsed_files = 0;
        let mut budget_exhausted = false;
        let mut unexpanded = Vec::new();

        for candidate in &candidates {
            if budget_exhausted
                || !candidate.has_language
                || candidate.size > MAX_FILE_SIZE
                || parsed_files >= MAX_FILES_PARSED
                || output.len() >= expanded_budget
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

            if output.len() + block.len() > expanded_budget {
                budget_exhausted = true;
                unexpanded.push(candidate.display_path.as_str());
                continue;
            }

            output.push_str(&block);
            expanded_files += 1;
        }

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
        let _ = write!(
            output,
            "\n{expanded_files} of {total_files} files expanded with symbols, \
             {listed_paths} listed by path only",
        );
        if omitted > 0 {
            let _ = write!(output, ", {omitted} omitted to stay within the byte budget");
        }
        output.push_str(".\n");

        Ok(output)
    }
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
) -> Option<String> {
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

    let mut block = String::new();
    block.push_str(&candidate.display_path);
    block.push('\n');
    for item in items.iter().take(MAX_SYMBOLS_PER_FILE) {
        if item.text.is_empty() {
            continue;
        }
        block.push_str("  ");
        for _ in 0..item.depth {
            block.push(' ');
        }
        block.push_str(&item.text);
        let start_row = item.range.start.row + 1;
        let end_row = item.range.end.row + 1;
        if start_row == end_row {
            let _ = writeln!(block, " [L{start_row}]");
        } else {
            let _ = writeln!(block, " [L{start_row}-{end_row}]");
        }
    }
    if items.len() > MAX_SYMBOLS_PER_FILE {
        let _ = writeln!(
            block,
            "  … {} more symbols",
            items.len() - MAX_SYMBOLS_PER_FILE
        );
    }
    block.push('\n');
    Some(block)
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
                format!("{path} のリポジトリマップを作成").into()
            }
            None => "リポジトリマップを作成".into(),
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
        assert!(output.contains("fn new"), "{output}");
        assert!(output.contains("[L"), "{output}");
        assert!(
            output.contains("1 of 1 files expanded with symbols"),
            "{output}"
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
