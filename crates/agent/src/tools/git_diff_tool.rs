use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use git::patch_summary::{self, FileChangeKind, PatchSummaryOptions};
use git::repository::{DiffType, RepoPath};
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use language::{OutlineItem, Point};
use project::{Project, ProjectPath, WorktreeSettings, git_store::Repository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use std::ops::Range;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;

/// Shows what changed in the project's git repository: a table of the changed
/// files with the lines each one adds and removes, the symbols the changes
/// touch, and as much of the patch as fits in `max_bytes`.
///
/// By default it compares the working tree with the merge base of the current
/// branch and the repository's default branch, which is everything the branch
/// changes, committed or not. Pass `base: "HEAD"` for only the uncommitted
/// changes. Untracked files are not included.
///
/// When the patch is cut to fit, narrow it down with `path` (a file or a
/// directory) rather than raising `max_bytes`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitDiffToolInput {
    /// Optional file or directory to restrict the diff to. Like other tools,
    /// the first component must be a project root directory.
    ///
    /// <example>
    /// If the project has a root directory `my-app`, pass `my-app/src/lib.rs`
    /// for that file's changes. Omit this field for the whole repository.
    /// </example>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What to compare the working tree with: a branch, tag or commit. Omit it
    /// for the merge base with the repository's default branch; pass `HEAD`
    /// for the uncommitted changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Maximum size of the result in bytes. Defaults to 16384 (roughly 4k
    /// tokens). Values are clamped to the range 1024..=262144.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

pub struct GitDiffTool {
    project: Entity<Project>,
}

impl GitDiffTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

const DEFAULT_MAX_BYTES: usize = 16 * 1024;
const MIN_MAX_BYTES: usize = 1024;
const HARD_MAX_BYTES: usize = 256 * 1024;

/// Files whose changed symbols are looked up; opening and parsing every file
/// of a very large diff would cost more than the list is worth.
const MAX_FILES_WITH_SYMBOLS: usize = 50;
const MAX_SYMBOLS_PER_FILE: usize = 20;
const MAX_FILE_SIZE_FOR_SYMBOLS: u64 = 1024 * 1024;

/// How a diff summary is titled and bounded.
pub struct GitDiffSummaryRequest {
    pub title: String,
    pub full_diff_hint: String,
    pub include_patch: bool,
    pub max_bytes: usize,
}

impl GitDiffSummaryRequest {
    /// The summary of `git diff --merge-base <base_ref>`, the branch diff that
    /// chat attachments and the git panel's review send.
    pub fn branch(base_ref: &str, include_patch: bool, max_bytes: usize) -> Self {
        Self {
            title: title_for_base(base_ref),
            full_diff_hint: full_diff_hint(base_ref),
            include_patch,
            max_bytes,
        }
    }
}

fn title_for_base(base: &str) -> String {
    if base == "HEAD" {
        "Uncommitted changes (the working tree against `HEAD`)".to_string()
    } else {
        format!("Changes since the merge base with `{base}`")
    }
}

fn full_diff_hint(base: &str) -> String {
    format!(
        "For more of a file's diff, use the `git_diff` tool with its `path` if you have it, \
         or run `git diff --merge-base {base} -- <path>`."
    )
}

/// Summarizes `patch` (the output of `git diff` in `repository`) for a
/// language model: the file table, the symbols each file's hunks touch in the
/// file as it is now, and the patch cut to fit `request.max_bytes`.
pub async fn summarize_git_diff(
    project: Entity<Project>,
    repository: Entity<Repository>,
    patch: &str,
    request: GitDiffSummaryRequest,
    cx: &mut AsyncApp,
) -> String {
    let symbols = changed_symbols(&project, &repository, patch, cx).await;
    patch_summary::render_patch_summary(
        patch,
        &symbols,
        &PatchSummaryOptions {
            title: &request.title,
            full_diff_hint: &request.full_diff_hint,
            include_patch: request.include_patch,
            max_bytes: request.max_bytes,
        },
    )
}

/// A branch diff (`diff_text`, from `repository`) as it is attached for the
/// agent: without the files it may not see (see [`withhold_hidden_files`]),
/// summarized as `request` asks or, without a request, the rest of the patch
/// in full, and followed by a line saying how many files were left out.
pub async fn branch_diff_for_agent(
    project: Entity<Project>,
    repository: Entity<Repository>,
    diff_text: &str,
    request: Option<GitDiffSummaryRequest>,
    cx: &mut AsyncApp,
) -> String {
    let (patch, withheld) = cx.update(|cx| withhold_hidden_files(diff_text, &repository, cx));
    let note = withheld.note();
    let mut content = if patch.trim().is_empty() {
        "No changes found in branch diff.".to_string()
    } else {
        match request {
            Some(mut request) => {
                request.max_bytes = request.max_bytes.saturating_sub(note.len());
                summarize_git_diff(project, repository, &patch, request, cx).await
            }
            None => patch,
        }
    };
    content.push_str(&note);
    content
}

async fn changed_symbols(
    project: &Entity<Project>,
    repository: &Entity<Repository>,
    patch: &str,
    cx: &mut AsyncApp,
) -> HashMap<String, Vec<String>> {
    let files = patch_summary::parse_patch(patch);
    let targets: Vec<(String, Vec<Range<u32>>, ProjectPath)> = cx.update(|cx| {
        files
            .iter()
            .filter(|file| {
                !file.is_binary && file.kind != FileChangeKind::Deleted && !file.new_rows.is_empty()
            })
            .take(MAX_FILES_WITH_SYMBOLS)
            .filter_map(|file| {
                let repo_path = RepoPath::new(&file.path).ok()?;
                let project_path = repository
                    .read(cx)
                    .repo_path_to_project_path(&repo_path, cx)?;
                let size = project.read(cx).entry_for_path(&project_path, cx)?.size;
                (size <= MAX_FILE_SIZE_FOR_SYMBOLS)
                    .then(|| (file.path.clone(), file.new_rows.clone(), project_path))
            })
            .collect()
    });

    let mut symbols = HashMap::default();
    for (path, rows, project_path) in targets {
        let buffer = project.update(cx, |project, cx| project.open_buffer(project_path, cx));
        let Ok(buffer) = buffer.await else {
            continue;
        };
        buffer
            .read_with(cx, |buffer, _| buffer.parsing_idle())
            .await;
        let items = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .outline(None)
                .items
                .into_iter()
                .map(|item| item.to_point(&snapshot))
                .collect::<Vec<_>>()
        });
        let touched = symbols_touched_by(&items, &rows);
        if !touched.is_empty() {
            symbols.insert(path, touched);
        }
    }
    symbols
}

/// The innermost outline items each row range falls in, in outline order:
/// a hunk inside a method names the method rather than its `impl` as well.
fn symbols_touched_by(items: &[OutlineItem<Point>], rows: &[Range<u32>]) -> Vec<String> {
    let intersects = |item: &OutlineItem<Point>, rows: &Range<u32>| {
        item.range.start.row < rows.end && rows.start <= item.range.end.row
    };
    let mut touched = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let is_innermost_for_some_hunk = rows.iter().any(|rows| {
            intersects(item, rows)
                && !items.iter().enumerate().any(|(other_index, other)| {
                    other_index != index
                        && other.depth > item.depth
                        && other.range.start >= item.range.start
                        && other.range.end <= item.range.end
                        && intersects(other, rows)
                })
        });
        if is_innermost_for_some_hunk {
            let start = item.range.start.row + 1;
            let end = item.range.end.row + 1;
            let label = if start == end {
                format!("{} [L{start}]", item.text)
            } else {
                format!("{} [L{start}-{end}]", item.text)
            };
            if !touched.contains(&label) {
                touched.push(label);
            }
        }
    }
    if touched.len() > MAX_SYMBOLS_PER_FILE {
        let more = touched.len() - MAX_SYMBOLS_PER_FILE;
        touched.truncate(MAX_SYMBOLS_PER_FILE);
        touched.push(format!("… {more} more"));
    }
    touched
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HiddenReason {
    Excluded,
    Private,
}

impl HiddenReason {
    fn setting(self) -> &'static str {
        match self {
            HiddenReason::Excluded => "file_scan_exclusions",
            HiddenReason::Private => "private_files",
        }
    }
}

/// Why the agent may not see `project_path`, mirroring the checks of the
/// `read_file` tool, or `None` when it may.
fn hidden_by_settings(project_path: &ProjectPath, cx: &App) -> Option<HiddenReason> {
    let global_settings = WorktreeSettings::get_global(cx);
    let worktree_settings = WorktreeSettings::get(Some(project_path.into()), cx);
    if global_settings.is_path_excluded(&project_path.path)
        || worktree_settings.is_path_excluded(&project_path.path)
    {
        Some(HiddenReason::Excluded)
    } else if global_settings.is_path_private(&project_path.path)
        || worktree_settings.is_path_private(&project_path.path)
    {
        Some(HiddenReason::Private)
    } else {
        None
    }
}

/// The changed files left out of a diff shown to the agent, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WithheldFiles {
    pub outside_project: usize,
    pub private: usize,
    pub excluded: usize,
}

impl WithheldFiles {
    pub fn total(&self) -> usize {
        self.outside_project + self.private + self.excluded
    }

    /// One line telling the model how many files were left out and why. Their
    /// paths are not given: the name of a private file can itself be
    /// sensitive.
    pub fn note(&self) -> String {
        let total = self.total();
        if total == 0 {
            return String::new();
        }
        let reasons = [
            (self.outside_project, "outside the project"),
            (self.private, "private"),
            (self.excluded, "excluded by settings"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, reason)| format!("{count} {reason}"))
        .collect::<Vec<_>>()
        .join(", ");
        format!(
            "\n{total} changed file{} left out ({reasons}); {} not shown.\n",
            if total == 1 { " was" } else { "s were" },
            if total == 1 {
                "its path and changes are"
            } else {
                "their paths and changes are"
            },
        )
    }
}

/// Removes from `patch` the files the agent could not read with its other
/// tools: files outside the project (the repository's work tree can be larger
/// than the project, e.g. a project opened in a subdirectory, or a home
/// directory under git) and files matching `private_files` or
/// `file_scan_exclusions`.
pub fn withhold_hidden_files(
    patch: &str,
    repository: &Entity<Repository>,
    cx: &App,
) -> (String, WithheldFiles) {
    let repository = repository.read(cx);
    let mut withheld = WithheldFiles::default();
    let (kept, _) = patch_summary::retain_files(patch, |path| {
        let Some(project_path) = RepoPath::new(path)
            .ok()
            .and_then(|repo_path| repository.repo_path_to_project_path(&repo_path, cx))
        else {
            withheld.outside_project += 1;
            return false;
        };
        match hidden_by_settings(&project_path, cx) {
            Some(HiddenReason::Private) => {
                withheld.private += 1;
                false
            }
            Some(HiddenReason::Excluded) => {
                withheld.excluded += 1;
                false
            }
            None => true,
        }
    });
    (kept, withheld)
}

/// `base` is handed to `git diff` as an argument, so anything that could be
/// read as an option or as more than one revision is refused.
fn validate_base(base: &str) -> Result<()> {
    anyhow::ensure!(!base.is_empty(), "`base` is empty.");
    anyhow::ensure!(
        !base.starts_with('-'),
        "`base` must be a branch, tag or commit, not an option: {base}"
    );
    anyhow::ensure!(
        !base.contains("..")
            && base
                .chars()
                .all(|character| character.is_alphanumeric() || "._-/@^~{}+".contains(character)),
        "`base` must be a single revision such as `main`, `origin/main`, `HEAD~3` or a commit hash: {base}"
    );
    Ok(())
}

impl GitDiffTool {
    async fn diff(
        project: Entity<Project>,
        input: GitDiffToolInput,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let max_bytes = input
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(MIN_MAX_BYTES, HARD_MAX_BYTES);
        let base = input.base.as_deref().map(str::trim);
        if let Some(base) = base {
            validate_base(base)?;
        }
        let path = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty() && *path != "." && *path != "./");

        let (repository, scope) = project.update(cx, |project, cx| match path {
            Some(path) => {
                let project_path = project
                    .find_project_path(path, cx)
                    .ok_or_else(|| anyhow!("{path} is not in the project."))?;
                if let Some(reason) = hidden_by_settings(&project_path, cx) {
                    anyhow::bail!(
                        "Cannot show the diff of {path} because its path matches the `{}` setting.",
                        reason.setting()
                    );
                }
                let (repository, repo_path) = project
                    .git_store()
                    .read(cx)
                    .repository_and_path_for_project_path(&project_path, cx)
                    .ok_or_else(|| anyhow!("{path} is not inside a git repository."))?;
                anyhow::Ok((repository, Some(repo_path)))
            }
            None => {
                let repository = project
                    .active_repository(cx)
                    .ok_or_else(|| anyhow!("The project has no git repository."))?;
                anyhow::Ok((repository, None))
            }
        })?;

        let base = match base {
            Some(base) => base.to_string(),
            None => repository
                .update(cx, |repository, _| repository.default_branch(false))
                .await
                .context("the repository went away")??
                .map(|branch| branch.to_string())
                .ok_or_else(|| {
                    anyhow!(
                        "Could not determine the repository's default branch; \
                         pass `base` explicitly (for example `main`, or `HEAD` for uncommitted changes)."
                    )
                })?,
        };

        let patch = repository
            .update(cx, |repository, cx| {
                repository.diff(
                    DiffType::MergeBase {
                        base_ref: base.clone().into(),
                    },
                    cx,
                )
            })
            .await
            .context("the repository went away")??;
        let patch = match &scope {
            Some(repo_path) => patch_summary::filter_patch(&patch, repo_path.as_unix_str()),
            None => patch,
        };
        let (patch, withheld) = cx.update(|cx| withhold_hidden_files(&patch, &repository, cx));
        let hidden_note = withheld.note();

        if patch.trim().is_empty() {
            let scope = match &scope {
                Some(repo_path) if !repo_path.is_empty() => {
                    format!(" in `{}`", repo_path.as_unix_str())
                }
                _ => String::new(),
            };
            return Ok(format!(
                "{}{scope}: no changes.{hidden_note}",
                title_for_base(&base)
            ));
        }

        let mut summary = summarize_git_diff(
            project,
            repository,
            &patch,
            GitDiffSummaryRequest::branch(&base, true, max_bytes.saturating_sub(hidden_note.len())),
            cx,
        )
        .await;
        summary.push_str(&hidden_note);
        Ok(summary)
    }
}

impl AgentTool for GitDiffTool {
    type Input = GitDiffToolInput;
    type Output = String;

    const NAME: &'static str = "git_diff";

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
                format!("{path} の変更 (git diff) を確認").into()
            }
            None => "変更 (git diff) を確認".into(),
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
            Self::diff(project, input, cx)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
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
        input: GitDiffToolInput,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let tool = Arc::new(GitDiffTool::new(project));
        cx.update(|cx| {
            tool.run(
                ToolInput::resolved(input),
                ToolCallEventStream::test().0,
                cx,
            )
        })
        .await
    }

    async fn project_with_repository(cx: &mut TestAppContext) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "src": { "lib.rs": "fn main() {}\n" },
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.executor().run_until_parked();
        project
    }

    #[test]
    fn test_validate_base_refuses_options_and_ranges() {
        for base in [
            "main",
            "origin/main",
            "HEAD",
            "HEAD~3",
            "v1.2.3",
            "a1b2c3d",
            "機能/分岐",
        ] {
            assert!(validate_base(base).is_ok(), "{base} should be accepted");
        }
        for base in [
            "",
            "--output=/tmp/x",
            "-p",
            "main..HEAD",
            "main HEAD",
            "main;rm",
            "HEAD:src",
            "main\0",
            "../main",
            "C:\\main",
            "--output",
            "main\n--output=x",
        ] {
            assert!(validate_base(base).is_err(), "{base:?} should be refused");
        }
    }

    #[gpui::test]
    async fn test_git_diff_refuses_an_option_as_base(cx: &mut TestAppContext) {
        init_test(cx);
        let project = project_with_repository(cx).await;
        let error = run_tool(
            project,
            GitDiffToolInput {
                path: None,
                base: Some("--output=/tmp/pwned".into()),
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap_err();
        assert!(error.contains("not an option"), "{error}");
    }

    #[gpui::test]
    async fn test_git_diff_without_a_repository_fails(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/plain"), json!({ "a.txt": "a" }))
            .await;
        let project = Project::test(fs, [path!("/plain").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let error = run_tool(
            project.clone(),
            GitDiffToolInput {
                path: None,
                base: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap_err();
        assert!(error.contains("no git repository"), "{error}");

        let error = run_tool(
            project,
            GitDiffToolInput {
                path: Some("plain/a.txt".into()),
                base: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap_err();
        assert!(error.contains("not inside a git repository"), "{error}");
    }

    #[gpui::test]
    async fn test_git_diff_reports_no_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let project = project_with_repository(cx).await;

        let output = run_tool(
            project.clone(),
            GitDiffToolInput {
                path: None,
                base: None,
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap();
        assert_eq!(
            output,
            "Changes since the merge base with `main`: no changes."
        );

        let output = run_tool(
            project,
            GitDiffToolInput {
                path: Some("project/src".into()),
                base: Some("HEAD".into()),
                max_bytes: Some(1),
            },
            cx,
        )
        .await
        .unwrap();
        assert_eq!(
            output,
            "Uncommitted changes (the working tree against `HEAD`) in `src`: no changes."
        );
    }

    #[gpui::test]
    async fn test_git_diff_hides_files_the_agent_cannot_read(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/home"),
            json!({
                ".git": {},
                ".ssh": { "config": "Host *\n" },
                "project": {
                    ".env": "TOKEN=1\n",
                    "src": { "lib.rs": "fn main() {}\n" },
                },
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/home/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .expect("the repository above the project is found");
        let file = |path: &str| {
            format!(
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n"
            )
        };
        let patch = [
            file("project/src/lib.rs"),
            file("project/.env"),
            file(".ssh/config"),
            file("project/.DS_Store"),
        ]
        .concat();
        let (kept, withheld) = cx.update(|cx| withhold_hidden_files(&patch, &repository, cx));
        assert_eq!(
            withheld,
            WithheldFiles {
                outside_project: 1,
                private: 1,
                excluded: 1,
            },
            "{kept}"
        );
        assert_eq!(kept, file("project/src/lib.rs"));
        let note = withheld.note();
        assert_eq!(
            note,
            "\n3 changed files were left out (1 outside the project, 1 private, \
             1 excluded by settings); their paths and changes are not shown.\n"
        );
        assert!(
            !note.contains(".env") && !note.contains(".ssh") && !note.contains("DS_Store"),
            "{note}"
        );
        assert_eq!(WithheldFiles::default().note(), "");

        // Branch diff attachments and the git panel's review go through the
        // same filter, in full and summarized.
        let full = cx
            .spawn({
                let project = project.clone();
                let repository = repository.clone();
                let patch = patch.clone();
                async move |mut cx| {
                    branch_diff_for_agent(project, repository, &patch, None, &mut cx).await
                }
            })
            .await;
        assert_eq!(full, format!("{}{note}", file("project/src/lib.rs")));
        let summary = cx
            .spawn({
                let project = project.clone();
                let repository = repository.clone();
                let patch = patch.clone();
                async move |mut cx| {
                    branch_diff_for_agent(
                        project,
                        repository,
                        &patch,
                        Some(GitDiffSummaryRequest::branch("main", true, 16 * 1024)),
                        &mut cx,
                    )
                    .await
                }
            })
            .await;
        assert!(summary.contains("project/src/lib.rs"), "{summary}");
        assert!(summary.ends_with(&note), "{summary}");
        for hidden in [".env", ".ssh", "DS_Store"] {
            assert!(!summary.contains(hidden), "{hidden} in {summary}");
        }
        let only_hidden = [file("project/.env"), file(".ssh/config")].concat();
        let nothing_left = cx
            .spawn({
                let project = project.clone();
                let repository = repository.clone();
                async move |mut cx| {
                    branch_diff_for_agent(project, repository, &only_hidden, None, &mut cx).await
                }
            })
            .await;
        assert!(
            nothing_left
                .starts_with("No changes found in branch diff.\n2 changed files were left out"),
            "{nothing_left}"
        );

        for outside in [
            "project/../.ssh/config",
            path!("/home/.ssh/config"),
            "../.ssh",
        ] {
            let error = run_tool(
                project.clone(),
                GitDiffToolInput {
                    path: Some(outside.into()),
                    base: Some("HEAD".into()),
                    max_bytes: None,
                },
                cx,
            )
            .await
            .unwrap_err();
            assert!(error.contains("not in the project"), "{outside}: {error}");
        }

        let error = run_tool(
            project,
            GitDiffToolInput {
                path: Some("project/.env".into()),
                base: Some("HEAD".into()),
                max_bytes: None,
            },
            cx,
        )
        .await
        .unwrap_err();
        assert!(error.contains("`private_files`"), "{error}");
    }

    fn outline_item(depth: usize, text: &str, rows: Range<u32>) -> OutlineItem<Point> {
        OutlineItem {
            depth,
            range: Point::new(rows.start, 0)..Point::new(rows.end, 0),
            text: text.to_string().into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_symbols_touched_by_names_the_innermost_items() {
        let items = vec![
            outline_item(0, "impl Config", 0..20),
            outline_item(1, "fn new", 1..5),
            outline_item(1, "fn merge", 6..15),
            outline_item(0, "fn helper", 22..30),
        ];
        assert_eq!(
            symbols_touched_by(&items, &[7..8]),
            vec!["fn merge [L7-16]".to_string()]
        );
        assert_eq!(
            symbols_touched_by(&items, &[16..18, 25..26]),
            vec![
                "impl Config [L1-21]".to_string(),
                "fn helper [L23-31]".to_string()
            ]
        );
        assert!(symbols_touched_by(&items, &[40..41]).is_empty());
    }
}
