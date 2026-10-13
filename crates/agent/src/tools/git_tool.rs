use super::deserialize_maybe_stringified;
use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use gpui::{App, AsyncApp, Entity, SharedString, Task, WeakEntity};
use project::{Project, git_store::Repository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;

/// Runs one git command in one of the project's repositories, so that you can change the
/// repository itself: stage (`add`, `reset`), commit, create, switch and delete branches,
/// merge, rebase, cherry-pick, revert, tag and apply patches. It runs outside the terminal
/// sandbox, where `.git` stays read-only.
///
/// - The user is asked to approve each call unless they allowed it in advance. Only commit,
///   create branches or otherwise change history when the user asked for it.
/// - Repository hooks never run (pre-commit, commit-msg, post-checkout, …). If the project
///   relies on them, for example to format or lint before a commit, run those checks yourself
///   with the `terminal` tool first.
/// - Nothing reaches the network: `fetch`, `pull`, `push`, `clone` and submodule updates are
///   not available. Tell the user the exact command and ask them to run it.
/// - No editor or pager is opened: give commit and tag messages with `-m`. Interactive
///   modes (`add -p`, `add -i`, `checkout -p`, `reset -p`, …) are not available. A message,
///   or a path after `--`, that starts with `-` is refused: reword it.
/// - Only repositories the user opened are available: one whose top-level directory is a
///   project root directory or contains one, and that was there when the project was opened
///   (or that the user created in the editor). A repository nested inside a project folder (a
///   submodule, or one created there) or created during the session is refused; ask the user to
///   run the command.
/// - Available subcommands: add, apply, branch, checkout, cherry-pick, commit, describe, diff,
///   for-each-ref, format-patch, log, ls-files, merge, rebase, reset, rev-parse, revert, show,
///   switch, tag. `status` is not one of them: use `git status` in the `terminal` tool, or the
///   `git_diff` tool, to see what changed.
/// - The command runs in the repository's top-level directory, so paths in `args` are relative
///   to it, not prefixed with the project's root directory name.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitToolInput {
    /// The arguments to git, starting with the subcommand and without `git` itself. Each
    /// argument is one element; nothing is split on spaces or interpreted by a shell.
    ///
    /// <example>
    /// `["commit", "-m", "Fix the parser's handling of empty input"]`
    /// </example>
    #[serde(deserialize_with = "deserialize_maybe_stringified")]
    pub args: Vec<String>,
    /// Optional path of a file or directory in the repository to run the command in, which picks
    /// the repository when the project has more than one. Like other tools, the first component
    /// must be a project root directory. Omit it for the project's active repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum size of the result in bytes. Defaults to 16384 (roughly 4k tokens). Values are
    /// clamped to the range 1024..=262144.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

pub struct GitTool {
    project: Entity<Project>,
    thread: WeakEntity<Thread>,
}

impl GitTool {
    pub fn new(project: Entity<Project>, thread: WeakEntity<Thread>) -> Self {
        Self { project, thread }
    }
}

const DEFAULT_MAX_BYTES: usize = 16 * 1024;
const MIN_MAX_BYTES: usize = 1024;
const HARD_MAX_BYTES: usize = 256 * 1024;

/// Subcommands that only read, before which the project's worktrees need not be paused
/// (ADR-F50). `branch` and `tag` create and delete refs, not files.
const READ_ONLY_GIT_COMMANDS: &[&str] = &[
    "branch",
    "describe",
    "diff",
    "for-each-ref",
    "log",
    "ls-files",
    "rev-parse",
    "show",
    "tag",
];

/// How the command is shown on the approval card and matched by the `git` tool permission
/// patterns: `git` and its arguments, each quoted the way a POSIX shell would need it. An
/// argument with a line break, another control character or an invisible formatting character
/// (which could reorder or hide what the card shows) is written as `$'…'` with each such
/// character escaped, so the card shows every argument on one line as it is.
pub fn git_command_text(args: &[String]) -> String {
    let mut text = String::from("git");
    for argument in args {
        text.push(' ');
        let plain = !argument.is_empty()
            && argument
                .chars()
                .all(|character| character.is_alphanumeric() || "-_./:=@^~+,%".contains(character));
        if plain {
            text.push_str(argument);
        } else if argument.chars().any(needs_escape) {
            text.push_str("$'");
            for character in argument.chars() {
                match character {
                    '\n' => text.push_str("\\n"),
                    '\r' => text.push_str("\\r"),
                    '\t' => text.push_str("\\t"),
                    '\\' => text.push_str("\\\\"),
                    '\'' => text.push_str("\\'"),
                    character if needs_escape(character) => {
                        text.push_str(&format!("\\u{:04X}", u32::from(character)));
                    }
                    character => text.push(character),
                }
            }
            text.push('\'');
        } else {
            text.push('\'');
            text.push_str(&argument.replace('\'', "'\\''"));
            text.push('\'');
        }
    }
    text
}

/// Characters [`git_command_text`] escapes: control characters, the line and paragraph separators,
/// and the zero-width and bidirectional formatting characters that change how the rest of the line
/// reads.
fn needs_escape(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{2029}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Whether the repository whose working tree is `work_directory` has its top-level directory at a
/// project root directory, or above one. A repository nested inside a project folder is not one
/// the user opened, whether a submodule or one the agent made itself with `git init` from its
/// sandbox, where it can write that repository's `.git/config` and so pick programs for git to
/// run outside the sandbox.
fn is_at_or_above_a_project_root(project: &Project, work_directory: &Path, cx: &App) -> bool {
    project
        .visible_worktrees(cx)
        .any(|worktree| worktree.read(cx).abs_path().starts_with(work_directory))
}

/// `text` cut to at most `max_bytes` on a character boundary, with how much was left out.
fn truncate_output(text: &str, max_bytes: usize) -> (&str, usize) {
    if text.len() <= max_bytes {
        return (text, 0);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}

/// The result the model reads: what ran, how it ended, and its output cut to `max_bytes`,
/// stderr first (git's diagnostics) so that a long stdout cannot push them out.
fn format_output(
    command: &str,
    work_directory: &Path,
    output: &git::repository::GitCommandOutput,
    max_bytes: usize,
) -> String {
    let status = match output.exit_code {
        Some(code) => format!("exited with status {code}"),
        None => "was terminated by a signal".to_string(),
    };
    let mut result = format!("`{command}` in `{}` {status}.\n", work_directory.display());
    let mut budget = max_bytes.saturating_sub(result.len());
    let mut cut = 0;
    for (name, text) in [("stderr", &output.stderr), ("stdout", &output.stdout)] {
        if text.trim().is_empty() {
            continue;
        }
        let (kept, left_out) = truncate_output(text, budget);
        budget -= kept.len();
        cut += left_out;
        result.push_str(&format!("\n{name}:\n```\n{kept}"));
        if !kept.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("```\n");
    }
    if output.stdout.trim().is_empty() && output.stderr.trim().is_empty() {
        result.push_str("\n(no output)\n");
    }
    if cut > 0 {
        result.push_str(&format!(
            "\n{cut} more bytes of output were cut. Narrow the command down (a path, `-n <count>`, \
             `--stat`) rather than raising `max_bytes`.\n"
        ));
    }
    result
}

impl GitTool {
    async fn run_git(
        project: Entity<Project>,
        thread: WeakEntity<Thread>,
        input: GitToolInput,
        event_stream: ToolCallEventStream,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        // The thread's switch is also what keeps this tool from the model, but a call already
        // on its way when the user turned it off must not run either.
        let enabled = thread
            .read_with(cx, |thread, _| thread.git_operations_enabled())
            .context("the thread went away")?;
        anyhow::ensure!(
            enabled,
            "The user has turned off Git operations for this thread. Tell the user the exact \
             command and ask them to run it."
        );

        let subcommand = git::repository::validate_agent_git_command(&input.args)?.to_string();
        let max_bytes = input
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(MIN_MAX_BYTES, HARD_MAX_BYTES);
        let path = input
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty() && *path != "." && *path != "./");

        let repository: Entity<Repository> = project.update(cx, |project, cx| match path {
            Some(path) => {
                let project_path = project
                    .find_project_path(path, cx)
                    .ok_or_else(|| anyhow!("{path} is not in the project."))?;
                let (repository, _) = project
                    .git_store()
                    .read(cx)
                    .repository_and_path_for_project_path(&project_path, cx)
                    .ok_or_else(|| anyhow!("{path} is not inside a git repository."))?;
                anyhow::Ok(repository)
            }
            None => project
                .active_repository(cx)
                .ok_or_else(|| anyhow!("The project has no git repository.")),
        })?;
        let work_directory = repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.clone()
        });
        let (at_or_above_a_project_root, opened_by_user) = project.read_with(cx, |project, cx| {
            (
                is_at_or_above_a_project_root(project, &work_directory, cx),
                project
                    .git_store()
                    .read(cx)
                    .repository_opened_by_user(&repository, cx),
            )
        });
        anyhow::ensure!(
            at_or_above_a_project_root,
            "The repository at `{}` is nested inside the project (a repository created in a \
             project folder, or a submodule), and the git tool only works in a repository the user \
             opened: one whose top-level directory is a project root directory or contains one. \
             Tell the user the exact command and ask them to run it.",
            work_directory.display()
        );
        // A repository that was not there when the folder was opened may have been made during
        // this session, by the agent from its sandbox, with a `.git/config` of its choosing.
        anyhow::ensure!(
            opened_by_user,
            "The repository at `{}` was created during this session (it was not there when the \
             project was opened, and the user did not create it in the editor), and the git tool \
             only works in a repository the user opened. Tell the user the exact command and ask \
             them to run it.",
            work_directory.display()
        );

        let command = git_command_text(&input.args);
        let authorize = cx.update(|cx| {
            let context = crate::ToolPermissionContext::new(Self::NAME, vec![command.clone()]);
            event_stream.authorize(
                i18n::t!("Run {}", MarkdownInlineCode(&command)),
                context,
                cx,
            )
        });
        authorize.await?;

        if !READ_ONLY_GIT_COMMANDS.contains(&subcommand.as_str()) {
            project
                .update(cx, |project, cx| {
                    project.note_agent_write_to_path(&work_directory, cx)
                })
                .await;
        }

        let output = repository
            .update(cx, |repository, _| {
                repository.run_git_command_without_hooks(input.args, None, HashMap::default())
            })
            .await
            .context("the repository went away")??;

        let result = format_output(&command, &work_directory, &output, max_bytes);
        if output.succeeded() {
            Ok(result)
        } else {
            Err(anyhow!(result))
        }
    }
}

impl AgentTool for GitTool {
    type Input = GitToolInput;
    type Output = String;

    const NAME: &'static str = "git";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => {
                let command = git_command_text(&input.args);
                i18n::t!("Run {}", MarkdownInlineCode(&command)).into()
            }
            Err(_) => i18n::t!("Run a git command").into(),
        }
    }

    /// An untrusted project's tools are cut down to ones that cannot change it.
    fn allow_in_restricted_mode() -> bool {
        false
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        let thread = self.thread.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            Self::run_git(project, thread, input, event_stream, cx)
                .await
                .map_err(|error| format!("{error:#}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextServerRegistry, Templates, ToolCallEventStreamReceiver};
    use fs::{FakeFs, Fs as _};
    use git::repository::GitCommandOutput;
    use gpui::{AppContext as _, TestAppContext};
    use language_model::LanguageModelRegistry;
    use prompt_store::ProjectContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn dot_git() -> &'static Path {
        Path::new(path!("/project/.git"))
    }

    async fn setup(cx: &mut TestAppContext) -> (Arc<FakeFs>, Entity<Project>, Entity<Thread>) {
        setup_with(
            path!("/project"),
            json!({
                ".git": {},
                "src": { "lib.rs": "fn main() {}\n" },
            }),
            path!("/project"),
            cx,
        )
        .await
    }

    async fn setup_with(
        tree_root: &str,
        tree: serde_json::Value,
        worktree: &str,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, Entity<Project>, Entity<Thread>) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(tree_root, tree).await;
        let project = Project::test(fs.clone(), [worktree.as_ref()], cx).await;
        cx.executor().run_until_parked();
        let model = cx.update(|cx| LanguageModelRegistry::test(cx).model("fake"));
        let context_server_registry =
            cx.new(|cx| ContextServerRegistry::new(project.read(cx).context_server_store(), cx));
        let thread = cx.new(|cx| {
            Thread::new(
                project.clone(),
                cx.new(|_cx| ProjectContext::default()),
                context_server_registry,
                Templates::new(),
                Some(model),
                cx,
            )
        });
        (fs, project, thread)
    }

    fn answer(fs: &FakeFs, args: &str, exit_code: i32, stdout: &str) {
        fs.with_git_state(dot_git(), false, |state| {
            state.command_outputs.insert(
                args.to_string(),
                GitCommandOutput {
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                    exit_code: Some(exit_code),
                },
            );
        })
        .unwrap();
    }

    /// The commands the tool ran, each with whether it went through the hook-free entry point.
    fn agent_runs(fs: &FakeFs) -> Vec<(String, bool)> {
        fs.with_git_state(dot_git(), false, |state| {
            state
                .run_command_log
                .iter()
                .zip(&state.run_command_without_hooks)
                // The repository's own scans run `rev-parse` and `diff` probes.
                .filter(|((args, _), _)| {
                    !matches!(args.first().map(String::as_str), Some("rev-parse" | "diff"))
                })
                .map(|((args, _), without_hooks)| (args.join(" "), *without_hooks))
                .collect()
        })
        .unwrap()
    }

    fn run_tool(
        project: &Entity<Project>,
        thread: &Entity<Thread>,
        args: &[&str],
        cx: &mut TestAppContext,
    ) -> (Task<Result<String, String>>, ToolCallEventStreamReceiver) {
        run_tool_in(project, thread, args, None, cx)
    }

    fn run_tool_in(
        project: &Entity<Project>,
        thread: &Entity<Thread>,
        args: &[&str],
        path: Option<&str>,
        cx: &mut TestAppContext,
    ) -> (Task<Result<String, String>>, ToolCallEventStreamReceiver) {
        let tool = Arc::new(GitTool::new(project.clone(), thread.downgrade()));
        let (event_stream, receiver) = ToolCallEventStream::test();
        let input = GitToolInput {
            args: args.iter().map(|argument| argument.to_string()).collect(),
            path: path.map(str::to_string),
            max_bytes: None,
        };
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));
        (task, receiver)
    }

    fn allow_once() -> acp_thread::SelectedPermissionOutcome {
        acp_thread::SelectedPermissionOutcome::new(
            acp::PermissionOptionId::new("allow"),
            acp::PermissionOptionKind::AllowOnce,
        )
    }

    #[test]
    fn test_git_command_text_quotes_what_a_shell_would_split() {
        let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(
            git_command_text(&args(&["commit", "-m", "Fix it"])),
            "git commit -m 'Fix it'"
        );
        assert_eq!(
            git_command_text(&args(&["commit", "-m", "it's", ""])),
            "git commit -m 'it'\\''s' ''"
        );
        assert_eq!(
            git_command_text(&args(&["reset", "--hard", "HEAD~1"])),
            "git reset --hard HEAD~1"
        );
        // Every argument stays on the card's one line, and nothing invisible reorders it.
        assert_eq!(
            git_command_text(&args(&["commit", "-m", "Fix it\n\nreset --hard", "a\tb"])),
            "git commit -m $'Fix it\\n\\nreset --hard' $'a\\tb'"
        );
        assert_eq!(
            git_command_text(&args(&["tag", "v1\u{202E}gnp.exe", "it's\\\r\u{7}"])),
            "git tag $'v1\\u202Egnp.exe' $'it\\'s\\\\\\r\\u0007'"
        );
        let text = git_command_text(&args(&[
            "commit",
            "-m",
            "x\u{200B}\u{2066}y\u{FEFF}z\u{2028}w\u{061C}",
        ]));
        assert!(
            !text.chars().any(|character| character.is_control()
                || matches!(
                    character,
                    '\u{200B}' | '\u{2066}' | '\u{FEFF}' | '\u{2028}' | '\u{061C}'
                )),
            "{text:?}"
        );
    }

    #[test]
    fn test_format_output_cuts_to_max_bytes() {
        let output = GitCommandOutput {
            stdout: "line\n".repeat(1000),
            stderr: "warning: something\n".to_string(),
            exit_code: Some(0),
        };
        let result = format_output("git log", Path::new("/repo"), &output, MIN_MAX_BYTES);
        assert!(result.len() <= MIN_MAX_BYTES + 200, "{}", result.len());
        assert!(result.contains("warning: something"), "{result}");
        assert!(result.contains("more bytes of output were cut"), "{result}");

        let empty = GitCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        };
        let result = format_output("git add a", Path::new("/repo"), &empty, MIN_MAX_BYTES);
        assert!(result.contains("terminated by a signal"), "{result}");
        assert!(result.contains("(no output)"), "{result}");

        let (kept, cut) = truncate_output("aé", 2);
        assert_eq!((kept, cut), ("a", 2), "cut on a character boundary");
    }

    #[gpui::test]
    async fn test_git_tool_runs_without_hooks_once_approved(cx: &mut TestAppContext) {
        let (fs, project, thread) = setup(cx).await;
        answer(&fs, "commit -m Fix it", 0, "[main 1234567] Fix it\n");

        let (task, mut receiver) = run_tool(&project, &thread, &["commit", "-m", "Fix it"], cx);
        let authorization = receiver.expect_authorization().await;
        let title = authorization
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_default();
        assert!(title.contains("git commit -m 'Fix it'"), "{title}");
        let context = authorization
            .context
            .expect("the git permission rules apply");
        assert_eq!(context.tool_name, GitTool::NAME);
        assert_eq!(
            context.input_values,
            vec!["git commit -m 'Fix it'".to_string()]
        );
        assert!(
            agent_runs(&fs).is_empty(),
            "nothing runs before the user approves"
        );
        authorization.response.send(allow_once()).unwrap();

        let output = task.await.unwrap();
        assert!(output.contains("exited with status 0"), "{output}");
        assert!(output.contains("[main 1234567] Fix it"), "{output}");
        assert_eq!(
            agent_runs(&fs),
            vec![("commit -m Fix it".to_string(), true)]
        );

        // A command git fails is a failed tool call, with git's output.
        answer(&fs, "switch main", 128, "");
        let (task, mut receiver) = run_tool(&project, &thread, &["switch", "main"], cx);
        receiver
            .expect_authorization()
            .await
            .response
            .send(allow_once())
            .unwrap();
        let error = task.await.unwrap_err();
        assert!(error.contains("exited with status 128"), "{error}");
    }

    #[gpui::test]
    async fn test_git_tool_refuses_before_asking(cx: &mut TestAppContext) {
        let (fs, project, thread) = setup(cx).await;
        for args in [
            &["push", "origin", "main"][..],
            &["fetch"],
            &["merge", "-s", "evil", "feature"],
            &["checkout", "--recurse-submodules", "main"],
            &["rebase", "--exec", "touch /tmp/pwned", "HEAD~2"],
            &["commit-tree", "HEAD^{tree}"],
            &[],
        ] {
            let (task, mut receiver) = run_tool(&project, &thread, args, cx);
            // Before awaiting the call, which would wait for an answer to a card it should not show.
            cx.run_until_parked();
            receiver.expect_no_authorization();
            let error = task.await.unwrap_err();
            assert!(
                error.contains("unsupported git command") || error.contains("no git subcommand"),
                "{args:?}: {error}"
            );
        }
        assert!(agent_runs(&fs).is_empty());
    }

    /// A repository nested in a project folder, which the agent may have made itself with its own
    /// `.git/config`, is not one the user opened, whichever way the tool comes to it.
    #[gpui::test]
    async fn test_git_tool_refuses_a_nested_repository(cx: &mut TestAppContext) {
        let (fs, project, thread) = setup_with(
            path!("/project"),
            json!({
                ".git": {},
                "src": { "lib.rs": "fn main() {}\n" },
                "vendor": {
                    "made": {
                        ".git": {},
                        "x.txt": "x",
                    },
                },
            }),
            path!("/project"),
            cx,
        )
        .await;
        let nested_dot_git = Path::new(path!("/project/vendor/made/.git"));
        fs.with_git_state(nested_dot_git, false, |state| {
            state.command_outputs.insert(
                "log -1".to_string(),
                GitCommandOutput {
                    stdout: "nested\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();
        answer(&fs, "log -1", 0, "root\n");

        for path in ["project/vendor/made/x.txt", "project/vendor/made"] {
            let (task, mut receiver) =
                run_tool_in(&project, &thread, &["log", "-1"], Some(path), cx);
            cx.run_until_parked();
            receiver.expect_no_authorization();
            let error = task.await.unwrap_err();
            assert!(
                error.contains("is nested inside the project"),
                "{path}: {error}"
            );
        }
        let nested_runs = fs
            .with_git_state(nested_dot_git, false, |state| {
                state
                    .run_command_log
                    .iter()
                    .filter(|(args, _)| args.first().map(String::as_str) == Some("log"))
                    .count()
            })
            .unwrap();
        assert_eq!(nested_runs, 0);

        // The root repository is the user's.
        for path in [Some("project/src/lib.rs"), None] {
            let (task, mut receiver) = run_tool_in(&project, &thread, &["log", "-1"], path, cx);
            receiver
                .expect_authorization()
                .await
                .response
                .send(allow_once())
                .unwrap();
            let output = task.await.unwrap();
            assert!(output.contains("root"), "{path:?}: {output}");
        }

        // The project's only repository, and so the active one, can be a nested one too.
        let (fs, project, thread) = setup_with(
            path!("/project"),
            json!({
                "src": { "lib.rs": "fn main() {}\n" },
                "made": { ".git": {}, "x.txt": "x" },
            }),
            path!("/project"),
            cx,
        )
        .await;
        let (task, mut receiver) = run_tool(&project, &thread, &["log", "-1"], cx);
        cx.run_until_parked();
        receiver.expect_no_authorization();
        let error = task.await.unwrap_err();
        assert!(error.contains("is nested inside the project"), "{error}");
        drop(fs);
    }

    fn answer_in(fs: &FakeFs, dot_git: &str, args: &str, stdout: &str) {
        fs.with_git_state(Path::new(dot_git), false, |state| {
            state.command_outputs.insert(
                args.to_string(),
                GitCommandOutput {
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();
    }

    /// A repository that appears after the project was opened may be one the agent made from its
    /// sandbox, `.git/config` and all, even at a project root, where nothing else tells it apart
    /// from the user's.
    #[gpui::test]
    async fn test_git_tool_refuses_a_repository_created_during_the_session(
        cx: &mut TestAppContext,
    ) {
        let (fs, project, thread) = setup_with(
            path!("/project"),
            json!({ "src": { "lib.rs": "fn main() {}\n" } }),
            path!("/project"),
            cx,
        )
        .await;
        fs.create_dir(Path::new(path!("/project/.git")))
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(
            project.read_with(cx, |project, cx| project.active_repository(cx).is_some()),
            "the new repository is found"
        );
        answer_in(&fs, path!("/project/.git"), "log -1", "made\n");

        let (task, mut receiver) = run_tool(&project, &thread, &["log", "-1"], cx);
        cx.run_until_parked();
        receiver.expect_no_authorization();
        let error = task.await.unwrap_err();
        assert!(error.contains("was created during this session"), "{error}");
        let runs = fs
            .with_git_state(Path::new(path!("/project/.git")), false, |state| {
                state
                    .run_command_log
                    .iter()
                    .filter(|(args, _)| args.first().map(String::as_str) == Some("log"))
                    .count()
            })
            .unwrap();
        assert_eq!(runs, 0);
    }

    /// The editor's own `git init` makes a repository the user opened.
    #[gpui::test]
    async fn test_git_tool_runs_in_a_repository_the_user_initialized(cx: &mut TestAppContext) {
        let (fs, project, thread) = setup_with(
            path!("/project"),
            json!({ "src": { "lib.rs": "fn main() {}\n" } }),
            path!("/project"),
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| {
                project.git_init(Path::new(path!("/project")).into(), "main".to_string(), cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        answer_in(&fs, path!("/project/.git"), "log -1", "initialized\n");

        let (task, mut receiver) = run_tool(&project, &thread, &["log", "-1"], cx);
        receiver
            .expect_authorization()
            .await
            .response
            .send(allow_once())
            .unwrap();
        let output = task.await.unwrap();
        assert!(output.contains("initialized"), "{output}");
    }

    /// A repository above the project's root directory, such as a project opened in a subfolder
    /// of a repository, is the user's.
    #[gpui::test]
    async fn test_git_tool_runs_in_a_repository_above_the_project(cx: &mut TestAppContext) {
        let (fs, project, thread) = setup_with(
            path!("/home"),
            json!({
                ".git": {},
                "project": { "src": { "lib.rs": "fn main() {}\n" } },
            }),
            path!("/home/project"),
            cx,
        )
        .await;
        fs.with_git_state(Path::new(path!("/home/.git")), false, |state| {
            state.command_outputs.insert(
                "log -1".to_string(),
                GitCommandOutput {
                    stdout: "above\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();
        let (task, mut receiver) = run_tool(&project, &thread, &["log", "-1"], cx);
        receiver
            .expect_authorization()
            .await
            .response
            .send(allow_once())
            .unwrap();
        let output = task.await.unwrap();
        assert!(output.contains("above"), "{output}");
    }

    #[gpui::test]
    async fn test_git_tool_refuses_in_a_thread_with_git_operations_off(cx: &mut TestAppContext) {
        let (fs, project, thread) = setup(cx).await;
        answer(&fs, "add src/lib.rs", 0, "");
        thread.update(cx, |thread, cx| {
            thread.set_git_operations_enabled(false, cx)
        });

        let (task, mut receiver) = run_tool(&project, &thread, &["add", "src/lib.rs"], cx);
        cx.run_until_parked();
        receiver.expect_no_authorization();
        let error = task.await.unwrap_err();
        assert!(error.contains("turned off Git operations"), "{error}");
        assert!(agent_runs(&fs).is_empty());
    }
}
