//! ADR-F49: what an external agent may have Zed write (`fs/write_text_file`).
//!
//! Zed saves such a file itself, with its own permissions and outside any
//! sandbox. Without these rules an agent whose commands are sandboxed could
//! still have Zed write `.git/config` (a `core.fsmonitor` that Zed's own
//! `git status` then runs on the host) or a shell startup file. The rules judge
//! the path; the write itself then goes through
//! `Project::save_buffer_verified`, which writes exactly that path or nothing,
//! so a link swapped in after the judgement can't redirect it.
//! [`authorize_agent_file_write`] is the one place every such write passes, so
//! ADR-F50's "agent write generation" bump belongs there too.

use crate::AcpThread;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Result, anyhow};
use fs::Fs;
use fs::verified_write::{
    AGENT_CONFIGURATION_HOME_RELATIVE, GIT_DIRECTORY_NAME, NameRules, UNIX_PROTECTED_HOME_RELATIVE,
};
use gpui::{App, AsyncApp, Entity, WeakEntity};
use project::Project;
use std::path::{Path, PathBuf};
use util::ResultExt as _;

/// A write the user has to allow every time, even when tool calls are
/// auto-approved, as the native agent's edit tools ask for these
/// (`authorize_always_prompt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SensitiveWrite {
    /// A project's `.zed` folder, whose settings can start programs.
    ProjectSettings,
    /// Zed's own settings: this application's settings directory, or the
    /// remote server's.
    Settings,
    /// An `.agents/skills` folder, whose instructions agents follow.
    AgentSkills,
    /// A project's configuration for an agent that runs on the host (MCP
    /// servers, hooks, permissions): AI must not configure MCP on its own.
    AgentConfiguration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    GitMetadata,
    ProtectedPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentWriteVerdict {
    Allow,
    Confirm(SensitiveWrite),
    Refuse(Refusal),
}

/// Where a write lands.
struct WriteTarget {
    /// The path Zed opens.
    opened: PathBuf,
    /// Where that leads once links are followed (`opened` itself when nothing
    /// on the way is one).
    resolved: PathBuf,
    /// The same path inside its worktree: as written, and as resolved. Names
    /// such as `.git` are only looked for here, so a project that sits in a
    /// folder with such a name is not refused as a whole.
    relative_spellings: Vec<PathBuf>,
    /// How the filesystem the file is on compares names.
    rules: NameRules,
}

/// The places, besides the project's Git metadata, that decide whether a write
/// is refused or needs the user's go-ahead.
enum ProtectedLocations {
    /// This machine, whose home and application directories are known.
    Local {
        home_directories: Vec<PathBuf>,
        settings_directories: Vec<PathBuf>,
        extra_protected_paths: Vec<PathBuf>,
    },
    /// A remote host. The client doesn't know its home directory, so every
    /// directory on the way to the file is treated as one that could be it.
    Remote,
}

// Mirrors `sandbox::protected_paths` (private to that crate), as the Unix list
// in `fs::verified_write` does.
#[cfg(target_os = "windows")]
const WINDOWS_PROTECTED_HOME_RELATIVE: &[&str] = &[
    r".ssh",
    r".gnupg",
    r"AppData\Roaming\gnupg",
    r".aws",
    r".azure",
    r".kube",
    r".docker\config.json",
    r".config\gh",
    r"AppData\Roaming\GitHub CLI",
    r".netrc",
    r"_netrc",
    r".git-credentials",
    r".gitconfig",
    r".config\git",
    r".claude.json",
    r".cargo\bin",
    r".local\bin",
    // Shims that are already on `PATH` for every shell.
    r"AppData\Roaming\npm",
    r"AppData\Local\Microsoft\WindowsApps",
    r"Documents\PowerShell",
    r"Documents\WindowsPowerShell",
    r".bashrc",
    r".bash_aliases",
    r".bash_profile",
    r".bash_login",
    r".profile",
    r".bash_logout",
    r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup",
    r".wslconfig",
];

#[cfg(target_os = "linux")]
const LOCAL_LOGIN_ITEMS_HOME_RELATIVE: &[&str] = &[".config/autostart", ".config/systemd/user"];
#[cfg(target_os = "macos")]
const LOCAL_LOGIN_ITEMS_HOME_RELATIVE: &[&str] = &["Library/LaunchAgents"];
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const LOCAL_LOGIN_ITEMS_HOME_RELATIVE: &[&str] = &[];

/// This machine's protected home entries. The user's own agent
/// configurations are among them only here: on a remote host, whose home is
/// unknown, the project-level files of that name get the user's go-ahead
/// instead.
fn local_protected_home_relative() -> impl Iterator<Item = &'static str> {
    #[cfg(target_os = "windows")]
    let platform_entries = WINDOWS_PROTECTED_HOME_RELATIVE;
    #[cfg(not(target_os = "windows"))]
    let platform_entries = UNIX_PROTECTED_HOME_RELATIVE;
    [
        platform_entries,
        AGENT_CONFIGURATION_HOME_RELATIVE,
        LOCAL_LOGIN_ITEMS_HOME_RELATIVE,
    ]
    .into_iter()
    .flat_map(|list| list.iter().copied())
}

/// The protected locations the environment moved away from their
/// home-relative defaults.
fn environment_protected_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    let entries: &[(&str, &str)] = &[
        ("APPDATA", "npm"),
        ("LOCALAPPDATA", r"Microsoft\WindowsApps"),
    ];
    #[cfg(not(target_os = "windows"))]
    let entries: &[(&str, &str)] = &[("XDG_CONFIG_HOME", "git")];
    entries
        .iter()
        .filter_map(|(variable, relative)| {
            let base = PathBuf::from(std::env::var_os(variable)?);
            base.is_absolute().then(|| base.join(relative))
        })
        .collect()
}

/// Checks a file an external agent asked Zed to write and, where the rules
/// call for it, asks the user first. Returns the card the user allowed the
/// write on, for the caller to close once the write is done; an error is the
/// answer to the agent when the write must not happen.
pub(crate) async fn authorize_agent_file_write(
    thread: &WeakEntity<AcpThread>,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<Option<acp::ToolCallId>> {
    let project = thread.read_with(cx, |thread, _| thread.project().clone())?;
    let (verdict, opened) = judge_agent_file_write(&project, path, cx).await?;
    let kind = match verdict {
        AgentWriteVerdict::Allow => return Ok(None),
        AgentWriteVerdict::Refuse(refusal) => return Err(anyhow!(refusal_message(refusal, path))),
        AgentWriteVerdict::Confirm(kind) => kind,
    };

    let title = confirmation_title(&opened, kind);
    let (tool_call_id, decision) = thread.update(cx, |thread, cx| {
        thread.request_zed_confirmation(title, acp::ToolKind::Edit, None, None, cx)
    })??;
    if !decision.await {
        return Err(anyhow!(
            "The user declined the write to `{}`.",
            path.display()
        ));
    }

    // The card can wait a long time, long enough for a sandboxed command to
    // swap a folder on the way for a link, so the path is judged again. What
    // changes after this, the verified write itself catches.
    let judged_again = match judge_agent_file_write(&project, path, cx).await {
        Ok((AgentWriteVerdict::Allow, _)) => Ok(()),
        Ok((AgentWriteVerdict::Confirm(now), _)) if now == kind => Ok(()),
        Ok((AgentWriteVerdict::Confirm(_), _)) => Err(anyhow!(changed_while_asking_message(path))),
        Ok((AgentWriteVerdict::Refuse(refusal), _)) => Err(anyhow!(refusal_message(refusal, path))),
        Err(error) => Err(error),
    };
    match judged_again {
        Ok(()) => Ok(Some(tool_call_id)),
        Err(error) => {
            thread
                .update(cx, |thread, cx| {
                    thread.finish_zed_confirmation(&tool_call_id, false, cx)
                })
                .and_then(|finished| finished)
                .log_err();
            Err(error)
        }
    }
}

/// What the rules say about a write to `path`, along with the path Zed would
/// open. An error is a refusal that comes before any rule: the path is not in
/// the project, or leads out of it.
async fn judge_agent_file_write(
    project: &Entity<Project>,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<(AgentWriteVerdict, PathBuf)> {
    let located = project
        .read_with(cx, |project, cx| locate_write(project, path, cx))
        .map_err(|message| anyhow!(message))?;
    if located.leaves_worktree {
        return Err(anyhow!(leaves_project_message(path)));
    }
    let rules = NameRules {
        windows: located.path_is_windows || NameRules::for_path(&located.opened).windows,
    };

    let mut relative_spellings = vec![located.relative.clone()];
    let (resolved, protected) = if located.is_local {
        let fs = project.read_with(cx, |project, _| project.fs().clone());
        let resolved = canonicalize_with_ancestors(fs.as_ref(), &located.opened)
            .await
            .ok_or_else(|| anyhow!(unverifiable_message(path)))?;
        let worktree_root = fs
            .canonicalize(&located.worktree_root)
            .await
            .map_err(|_| anyhow!(unverifiable_message(path)))?;
        let Some(resolved_relative) = rules.relative_to(&resolved, &worktree_root) else {
            return Err(anyhow!(leaves_project_message(path)));
        };
        relative_spellings.push(resolved_relative);
        (resolved, local_protected_locations(fs.as_ref()).await)
    } else {
        // A remote host's files can't be resolved from here, so a remote write
        // is judged by what the worktree's scan has seen; the remote server's
        // verified write catches the rest.
        let resolved = located
            .scanned_target
            .clone()
            .unwrap_or_else(|| located.opened.clone());
        relative_spellings.push(
            rules
                .relative_to(&resolved, &located.worktree_root)
                .unwrap_or_else(|| resolved.clone()),
        );
        (resolved, ProtectedLocations::Remote)
    };

    let target = WriteTarget {
        opened: located.opened,
        resolved,
        relative_spellings,
        rules,
    };
    let verdict = classify_agent_write(&target, &located.git_directories, &protected);
    Ok((verdict, target.opened))
}

/// Where a write to `path` lands in the project, as far as the worktree's scan
/// knows.
struct LocatedWrite {
    opened: PathBuf,
    relative: PathBuf,
    worktree_root: PathBuf,
    /// Where a symlink the scan has seen on the way leads.
    scanned_target: Option<PathBuf>,
    leaves_worktree: bool,
    git_directories: Vec<PathBuf>,
    is_local: bool,
    path_is_windows: bool,
}

fn locate_write(project: &Project, path: &Path, cx: &App) -> Result<LocatedWrite, String> {
    let project_path = project
        .project_path_for_absolute_path(path, cx)
        .ok_or_else(|| not_in_project_message(path))?;
    let worktree = project
        .worktree_for_id(project_path.worktree_id, cx)
        .ok_or_else(|| not_in_project_message(path))?;
    let worktree = worktree.read(cx);

    let mut scanned_target = None;
    let mut leaves_worktree = false;
    for ancestor in project_path.path.ancestors() {
        let Some(entry) = worktree.entry_for_path(ancestor) else {
            continue;
        };
        if entry.is_external {
            leaves_worktree = true;
            break;
        }
        if let Some(canonical_path) = &entry.canonical_path {
            let rest = project_path
                .path
                .strip_prefix(ancestor)
                .map_err(|_| unverifiable_message(path))?;
            scanned_target = Some(if rest.is_empty() {
                canonical_path.to_path_buf()
            } else {
                canonical_path.join(rest.as_std_path())
            });
            break;
        }
    }

    Ok(LocatedWrite {
        opened: worktree.absolutize(&project_path.path),
        relative: project_path.path.as_std_path().to_path_buf(),
        worktree_root: worktree.abs_path().to_path_buf(),
        scanned_target,
        leaves_worktree,
        git_directories: crate::sandbox_git_dirs(project, cx),
        is_local: project.is_local(),
        path_is_windows: project.path_style(cx).is_windows(),
    })
}

/// The canonical form of `path`: links followed in the part that exists, the
/// rest (a new file, or new folders on the way to it) kept as written.
async fn canonicalize_with_ancestors(fs: &dyn Fs, path: &Path) -> Option<PathBuf> {
    let mut missing_names = Vec::new();
    let mut current = Some(path);
    while let Some(ancestor) = current {
        if let Ok(mut canonical) = fs.canonicalize(ancestor).await {
            for name in missing_names.iter().rev() {
                canonical.push(name);
            }
            return Some(canonical);
        }
        missing_names.push(ancestor.file_name()?.to_os_string());
        current = ancestor.parent();
    }
    None
}

async fn local_protected_locations(fs: &dyn Fs) -> ProtectedLocations {
    let mut extra_protected_paths = Vec::new();
    let directories = [paths::data_dir().clone(), paths::state_dir().clone()]
        .into_iter()
        .chain(environment_protected_paths());
    for directory in directories {
        for spelling in with_canonical_spelling(fs, &directory).await {
            if !extra_protected_paths.contains(&spelling) {
                extra_protected_paths.push(spelling);
            }
        }
    }
    ProtectedLocations::Local {
        home_directories: with_canonical_spelling(fs, paths::home_dir()).await,
        settings_directories: with_canonical_spelling(fs, paths::config_dir()).await,
        extra_protected_paths,
    }
}

/// `path`, and also its canonical form when that is spelled differently, so
/// that both the path Zed opens and where it leads are compared against it.
async fn with_canonical_spelling(fs: &dyn Fs, path: &Path) -> Vec<PathBuf> {
    let mut spellings = vec![path.to_path_buf()];
    if let Some(canonical) = canonicalize_with_ancestors(fs, path).await
        && canonical != path
    {
        spellings.push(canonical);
    }
    spellings
}

fn classify_agent_write(
    target: &WriteTarget,
    git_directories: &[PathBuf],
    protected: &ProtectedLocations,
) -> AgentWriteVerdict {
    let rules = target.rules;
    let absolute = [target.opened.as_path(), target.resolved.as_path()];
    let relative = &target.relative_spellings;

    if relative
        .iter()
        .any(|path| rules.has_component_named(path, GIT_DIRECTORY_NAME))
        || absolute.iter().any(|path| {
            git_directories
                .iter()
                .any(|directory| rules.path_is_within(path, directory))
        })
    {
        return AgentWriteVerdict::Refuse(Refusal::GitMetadata);
    }

    match protected {
        ProtectedLocations::Local {
            home_directories,
            settings_directories,
            extra_protected_paths,
        } => {
            if absolute.iter().any(|path| {
                settings_directories
                    .iter()
                    .any(|directory| rules.path_is_within(path, directory))
            }) {
                return AgentWriteVerdict::Confirm(SensitiveWrite::Settings);
            }
            let protected_paths = home_directories
                .iter()
                .flat_map(|home| local_protected_home_relative().map(move |entry| home.join(entry)))
                .chain(extra_protected_paths.iter().cloned())
                .collect::<Vec<_>>();
            if absolute.iter().any(|path| {
                protected_paths
                    .iter()
                    .any(|protected_path| rules.path_is_within(path, protected_path))
            }) {
                return AgentWriteVerdict::Refuse(Refusal::ProtectedPath);
            }
        }
        ProtectedLocations::Remote => {
            if absolute
                .iter()
                .any(|path| rules.is_under_home_entry_anywhere(path, UNIX_PROTECTED_HOME_RELATIVE))
            {
                return AgentWriteVerdict::Refuse(Refusal::ProtectedPath);
            }
            // The remote server keeps its settings where Zed on Linux does.
            let remote_settings = format!(".config/{}", paths::APP_NAME_LOWERCASE);
            if absolute
                .iter()
                .any(|path| rules.is_under_home_entry_anywhere(path, &[remote_settings.as_str()]))
            {
                return AgentWriteVerdict::Confirm(SensitiveWrite::Settings);
            }
        }
    }

    if relative
        .iter()
        .any(|path| rules.has_component_named(path, paths::local_settings_folder_name()))
    {
        return AgentWriteVerdict::Confirm(SensitiveWrite::ProjectSettings);
    }
    if relative
        .iter()
        .any(|path| is_agent_configuration(path, rules))
    {
        return AgentWriteVerdict::Confirm(SensitiveWrite::AgentConfiguration);
    }
    if relative
        .iter()
        .any(|path| has_agent_skills_folder(path, rules))
    {
        return AgentWriteVerdict::Confirm(SensitiveWrite::AgentSkills);
    }
    AgentWriteVerdict::Allow
}

fn has_agent_skills_folder(path: &Path, rules: NameRules) -> bool {
    let agents = rules.comparable_name(agent_skills::AGENTS_DIR_NAME);
    let skills = rules.comparable_name(agent_skills::SKILLS_DIR_NAME);
    let names = rules.normal_component_keys(path).collect::<Vec<_>>();
    names
        .windows(2)
        .any(|pair| matches!(pair, [first, second] if *first == agents && *second == skills))
}

/// Whether `path` is part of a project's configuration for an agent that runs
/// on the host: anything in Claude Code's, the Gemini CLI's or Codex's project
/// folder (settings, and the hook and skill scripts the agent runs), or
/// `.mcp.json`.
fn is_agent_configuration(path: &Path, rules: NameRules) -> bool {
    let folders = AGENT_CONFIGURATION_HOME_RELATIVE
        .iter()
        .map(|folder| rules.comparable_name(folder))
        .collect::<Vec<_>>();
    let mcp_servers = rules.comparable_name(".mcp.json");
    rules
        .normal_component_keys(path)
        .any(|name| folders.contains(&name) || name == mcp_servers)
}

fn confirmation_title(path: &Path, kind: SensitiveWrite) -> String {
    let path = path.display();
    match kind {
        SensitiveWrite::ProjectSettings => format!("{path} を編集 (プロジェクトの設定)"),
        SensitiveWrite::Settings => format!("{path} を編集 (設定)"),
        SensitiveWrite::AgentSkills => format!("{path} を編集 (エージェントのスキル)"),
        SensitiveWrite::AgentConfiguration => format!("{path} を編集 (エージェントの設定)"),
    }
}

fn not_in_project_message(path: &Path) -> String {
    format!(
        "Zed refused to write `{}`: it is not inside any of the project's folders.",
        path.display()
    )
}

fn unverifiable_message(path: &Path) -> String {
    format!(
        "Zed refused to write `{}`: it could not check where that path leads.",
        path.display()
    )
}

fn leaves_project_message(path: &Path) -> String {
    format!(
        "Zed refused to write `{}`: it leads out of the project through a symbolic link or \
         junction. Agents may only write inside the project's folders.",
        path.display()
    )
}

fn changed_while_asking_message(path: &Path) -> String {
    format!(
        "Zed refused to write `{}`: where it leads changed while the user was deciding, so \
         what they allowed is not what would be written.",
        path.display()
    )
}

fn refusal_message(refusal: Refusal, path: &Path) -> String {
    match refusal {
        Refusal::GitMetadata => format!(
            "Zed refused to write `{}`: it is Git metadata. Agents may not change anything \
             inside a `.git` directory, because Git runs commands configured there. If the \
             change is really needed, show the user the exact change and ask them to make it.",
            path.display()
        ),
        Refusal::ProtectedPath => format!(
            "Zed refused to write `{}`: it holds credentials or configuration that runs \
             programs, runs at login, or belongs to Zed itself, and agents may not change such \
             files. If the change is really needed, show the user the exact change and ask them \
             to make it.",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::path;

    const WINDOWS: NameRules = NameRules { windows: true };
    const ELSEWHERE: NameRules = NameRules { windows: false };

    fn local() -> ProtectedLocations {
        ProtectedLocations::Local {
            home_directories: vec![PathBuf::from(path!("/home/user"))],
            settings_directories: vec![PathBuf::from(path!("/home/user/.config/zed-jp-enhance"))],
            extra_protected_paths: vec![PathBuf::from(path!(
                "/home/user/.local/share/zed-jp-enhance"
            ))],
        }
    }

    fn target_with(root: &str, relative: &str, rules: NameRules) -> WriteTarget {
        let opened = Path::new(root).join(relative);
        WriteTarget {
            resolved: opened.clone(),
            opened,
            relative_spellings: vec![PathBuf::from(relative)],
            rules,
        }
    }

    fn target(root: &str, relative: &str) -> WriteTarget {
        target_with(root, relative, NameRules::for_path(Path::new(root)))
    }

    fn classify(root: &str, relative: &str, protected: &ProtectedLocations) -> AgentWriteVerdict {
        classify_agent_write(&target(root, relative), &[], protected)
    }

    fn both() -> [ProtectedLocations; 2] {
        [local(), ProtectedLocations::Remote]
    }

    /// A name can hide invisible or compatibility characters, and any case may
    /// reach the real `.git` on a case-insensitive filesystem.
    #[test]
    fn git_metadata_is_refused_in_every_spelling() {
        for relative in [
            ".git/config",
            ".GIT/config",
            "sub/.git/hooks/pre-commit",
            ".gıt/config",
            ".g\u{200C}it/config",
            ".ｇit/config",
        ] {
            for rules in [WINDOWS, ELSEWHERE] {
                for protected in both() {
                    assert_eq!(
                        classify_agent_write(
                            &target_with(path!("/project"), relative, rules),
                            &[],
                            &protected
                        ),
                        AgentWriteVerdict::Refuse(Refusal::GitMetadata),
                        "{relative} ({rules:?})"
                    );
                }
            }
        }
    }

    /// Only Windows (and a Windows drive seen from WSL) reads `name:stream` as
    /// `name` and drops trailing dots and spaces; elsewhere these are other
    /// files, which may be written.
    #[test]
    fn windows_spellings_only_count_on_windows_filesystems() {
        for relative in [
            ".git:notes",
            ".git::$INDEX_ALLOCATION/config",
            ".git./config",
            ".git /config",
        ] {
            assert_eq!(
                classify_agent_write(
                    &target_with(path!("/project"), relative, WINDOWS),
                    &[],
                    &local()
                ),
                AgentWriteVerdict::Refuse(Refusal::GitMetadata),
                "{relative} on Windows"
            );
            assert_eq!(
                classify("/mnt/c/project", relative, &ProtectedLocations::Remote),
                AgentWriteVerdict::Refuse(Refusal::GitMetadata),
                "{relative} on a Windows drive in WSL"
            );
            assert_eq!(
                classify("/home/user/project", relative, &ProtectedLocations::Remote),
                AgentWriteVerdict::Allow,
                "{relative} on Linux"
            );
        }
    }

    #[test]
    fn a_repository_directory_is_refused_whatever_it_is_called() {
        let git_directories = [PathBuf::from(path!("/project/gitdata"))];
        for relative in ["gitdata/config", "GITDATA/hooks/pre-commit"] {
            assert_eq!(
                classify_agent_write(
                    &target(path!("/project"), relative),
                    &git_directories,
                    &local()
                ),
                AgentWriteVerdict::Refuse(Refusal::GitMetadata),
                "{relative}"
            );
        }
    }

    /// A path whose own spelling is harmless is judged by where it leads too.
    #[test]
    fn a_link_into_git_metadata_is_refused() {
        let mut target = target(path!("/project"), "safe/config");
        target.resolved = PathBuf::from(path!("/project/.git/config"));
        target.relative_spellings.push(PathBuf::from(".git/config"));
        assert_eq!(
            classify_agent_write(&target, &[], &local()),
            AgentWriteVerdict::Refuse(Refusal::GitMetadata)
        );
    }

    /// Names are only looked for inside the worktree, so a project that sits
    /// in a folder called `.git` or `.zed` is not refused as a whole.
    #[test]
    fn a_project_inside_a_folder_with_such_a_name_is_written() {
        for root in [path!("/work/.git/project"), path!("/work/.zed/project")] {
            for protected in both() {
                assert_eq!(
                    classify(root, "src/main.rs", &protected),
                    AgentWriteVerdict::Allow,
                    "{root}"
                );
            }
        }
    }

    #[test]
    fn settings_skills_and_agent_configurations_need_the_users_go_ahead() {
        for protected in both() {
            for (relative, kind) in [
                (".zed/settings.json", SensitiveWrite::ProjectSettings),
                (".ZED/tasks.json", SensitiveWrite::ProjectSettings),
                (
                    ".agents/skills/review/SKILL.md",
                    SensitiveWrite::AgentSkills,
                ),
                (
                    ".agents/s\u{212A}ills/review/SKILL.md",
                    SensitiveWrite::AgentSkills,
                ),
                (".claude/settings.json", SensitiveWrite::AgentConfiguration),
                (
                    ".claude/settings.local.json",
                    SensitiveWrite::AgentConfiguration,
                ),
                (
                    ".claude/hooks/pre-tool-use.sh",
                    SensitiveWrite::AgentConfiguration,
                ),
                (
                    ".claude/commands/review.md",
                    SensitiveWrite::AgentConfiguration,
                ),
                (".mcp.json", SensitiveWrite::AgentConfiguration),
                (".gemini/settings.json", SensitiveWrite::AgentConfiguration),
                (".codex/config.toml", SensitiveWrite::AgentConfiguration),
            ] {
                assert_eq!(
                    classify(path!("/project"), relative, &protected),
                    AgentWriteVerdict::Confirm(kind),
                    "{relative}"
                );
            }
        }
        assert_eq!(
            classify(
                path!("/home/user"),
                ".config/zed-jp-enhance/settings.json",
                &local()
            ),
            AgentWriteVerdict::Confirm(SensitiveWrite::Settings)
        );
        assert_eq!(
            classify(
                "/home/user",
                ".config/zed-jp-enhance/settings.json",
                &ProtectedLocations::Remote
            ),
            AgentWriteVerdict::Confirm(SensitiveWrite::Settings)
        );
    }

    #[test]
    fn ordinary_files_are_written_without_asking() {
        for relative in [
            "src/main.rs",
            ".gitignore",
            ".github/workflows/ci.yml",
            "docs/.git-notes.md",
            "skills/README.md",
        ] {
            for protected in both() {
                assert_eq!(
                    classify(path!("/project"), relative, &protected),
                    AgentWriteVerdict::Allow,
                    "{relative}"
                );
            }
        }
    }

    /// When the project is the home directory itself, the files a login, a
    /// credential lookup, Git or a host agent reads are in the project, and
    /// still refused.
    #[test]
    fn this_machines_protected_home_entries_are_refused() {
        for relative in [
            ".ssh/authorized_keys",
            ".SSH/authorized_keys",
            ".bashrc",
            ".cargo/bin/tool",
            ".gitconfig",
            ".config/git/config",
            ".claude.json",
            ".claude/settings.json",
            ".gemini/settings.json",
            ".local/share/zed-jp-enhance/db/threads.db",
        ] {
            assert_eq!(
                classify(path!("/home/user"), relative, &local()),
                AgentWriteVerdict::Refuse(Refusal::ProtectedPath),
                "{relative}"
            );
        }
        // A dotfiles repository elsewhere is the user's to change.
        assert_eq!(
            classify(path!("/home/user/dotfiles"), ".bashrc", &local()),
            AgentWriteVerdict::Allow
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_command_shims_are_refused() {
        for relative in [
            r"AppData\Roaming\npm\claude.cmd",
            r"AppData\Local\Microsoft\WindowsApps\python.exe",
        ] {
            assert_eq!(
                classify(path!("/home/user"), relative, &local()),
                AgentWriteVerdict::Refuse(Refusal::ProtectedPath),
                "{relative}"
            );
        }
    }

    #[test]
    fn on_a_remote_host_any_folder_could_be_the_home() {
        for (root, relative) in [
            ("/home/user", ".bashrc"),
            ("/srv/work", ".ssh/authorized_keys"),
            ("/home/user/project", ".profile"),
            ("/home/user", ".zed_server/zed-remote-server"),
            ("/home/user", ".config/fish/config.fish"),
            ("/home/user", ".gitconfig"),
            ("/home/user", ".config/git/config"),
        ] {
            assert_eq!(
                classify(root, relative, &ProtectedLocations::Remote),
                AgentWriteVerdict::Refuse(Refusal::ProtectedPath),
                "{root}/{relative}"
            );
        }
    }
}
