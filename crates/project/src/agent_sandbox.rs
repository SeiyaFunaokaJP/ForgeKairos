//! ADR-F49 第 2a 段: what an external agent needs from the sandbox it runs in —
//! its profile (where its configuration lives, which environment points it
//! there, which hosts it talks to) and its per-project configuration folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, ensure};
use collections::HashMap;
use fs::Fs;
use gpui::App;
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;

use crate::Project;
use crate::worktree_store::WorktreeStore;

/// How many bytes of the SHA-256 of a project's folders name its directory.
const PROJECT_KEY_BYTES: usize = 8;

/// Inside an agent's configuration folder: the directory given to it as
/// `HOME`. Everything the agent keeps below its home goes here rather than at
/// the top of the folder, which the remote helper keeps from taking new
/// entries when it cannot bind the names it protects there read-only.
const AGENT_HOME_FOLDER_NAME: &str = "home";

const CLAUDE_REGISTRY_ID: &str = "claude-acp";
const CODEX_REGISTRY_ID: &str = "codex-acp";
const GEMINI_REGISTRY_ID: &str = "gemini";

/// The project's directory name among the sandboxed agents' projects, from
/// the canonical paths of all its folders: the same folders, however they were
/// opened and in whatever order, always get the same configuration, and a
/// project with a folder more, fewer or other gets its own.
pub fn project_key(canonical_roots: &[PathBuf]) -> String {
    let mut roots = canonical_roots
        .iter()
        .map(|root| root.to_string_lossy())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    let mut hasher = Sha256::new();
    for root in roots {
        hasher.update(root.as_bytes());
        // No path holds a NUL, so each list hashes one way only.
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .take(PROJECT_KEY_BYTES)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn is_project_key(key: &str) -> bool {
    key.len() == PROJECT_KEY_BYTES * 2
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// ADR-F49 decision 15: the project's folders that [`project_key`] is taken
/// from, in one place — the sandboxed Claude Code CLI (第 1 段), an agent
/// started in this machine's sandbox (第 2a 段) and the folders sent to a
/// remote host all name a project the same way, so that the same project on
/// the same machine keeps one configuration folder and one sign-in.
///
/// Every visible worktree, a single file's by the folder holding it, spelled
/// as this platform spells them (on Windows the Windows spelling, not the
/// distribution's).
pub fn project_key_roots(worktree_store: &WorktreeStore, cx: &App) -> Vec<PathBuf> {
    Project::default_visible_worktree_paths(worktree_store, cx)
}

/// [`project_key_roots`] resolved through links, which is what the key is
/// taken from: the same folders opened directly and through a link are one
/// project. A folder that cannot be resolved (it went away, or a permission)
/// is kept as it was spelled.
pub async fn canonical_project_key_roots(fs: &Arc<dyn Fs>, roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut canonical_roots = Vec::with_capacity(roots.len());
    for root in roots {
        let canonical = fs.canonicalize(&root).await.log_err();
        canonical_roots.push(canonical.unwrap_or(root));
    }
    canonical_roots
}

/// Which profile an external agent runs under in the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SandboxedAgentProfile {
    /// Claude Agent (`claude-acp` in the ACP registry).
    Claude,
    /// Codex (`codex-acp`).
    Codex,
    /// Gemini CLI (`gemini`).
    Gemini,
    /// Any other agent: one configured as a command, or another registry
    /// entry.
    Other { id: String },
}

impl SandboxedAgentProfile {
    /// Only the registry's own entries get a vendor's profile: a command the
    /// user configured under one of their names could be anything.
    pub fn for_agent(id: &str, from_registry: bool) -> Self {
        match id {
            CLAUDE_REGISTRY_ID if from_registry => Self::Claude,
            CODEX_REGISTRY_ID if from_registry => Self::Codex,
            GEMINI_REGISTRY_ID if from_registry => Self::Gemini,
            _ => Self::Other { id: id.to_string() },
        }
    }

    /// The agent's folder in the project's directory (ADR-F49 決定 15, revised
    /// by ADR-F53).
    ///
    /// Claude's is **not** the sandboxed Claude Code CLI's (`config`): the ACP
    /// Claude agent drives the Agent SDK from this IDE's harness, which a
    /// Claude subscription's credentials may not be used for, so the two never
    /// share a folder and hence never share a sign-in. A folder the CLI signed
    /// in with is left exactly as it is; this route simply looks elsewhere.
    pub fn config_folder_name(&self) -> String {
        match self {
            Self::Claude => CLAUDE_REGISTRY_ID.to_string(),
            Self::Codex => "codex".to_string(),
            Self::Gemini => "gemini".to_string(),
            Self::Other { id } => format!("agent-{}", folder_safe_name(id)),
        }
    }

    /// The hosts the agent talks to, from each vendor's published code:
    /// Claude Agent 0.76 (the model API only — ADR-F53 keeps the OAuth token
    /// exchange `platform.claude.com` out, since this route is API-key only
    /// and reaching it is what a subscription sign-in needs), codex-acp 1.11
    /// on `@openai/codex` 0.153 (the API-key and ChatGPT backends, and the
    /// device-code sign-in), Gemini CLI 0.59 (Code Assist, Google's OAuth
    /// token and user-info endpoints, and the API-key endpoint). `None`:
    /// whatever the IDE-wide allowlist permits.
    pub fn hosts(&self) -> Option<&'static [&'static str]> {
        match self {
            Self::Claude => Some(&["api.anthropic.com"]),
            Self::Codex => Some(&["api.openai.com", "chatgpt.com", "auth.openai.com"]),
            Self::Gemini => Some(&[
                "cloudcode-pa.googleapis.com",
                "oauth2.googleapis.com",
                "www.googleapis.com",
                "generativelanguage.googleapis.com",
            ]),
            Self::Other { .. } => None,
        }
    }

    /// The environment that keeps the agent's state in `config_dir` and its
    /// sign-in on a flow that works from inside the sandbox.
    ///
    /// `HOME` is the folder's own home for every agent, so that neither the
    /// agent nor what it runs writes to, or looks for credentials in, the
    /// account's real home. `NO_BROWSER` has the vendor's agent offer the
    /// sign-in that ends with a code the user pastes (Gemini's user-code
    /// flow) or types on a page (Codex's device code) rather than one that
    /// waits for the browser on a localhost port, which the sandbox's network
    /// namespace does not share. ADR-F53: Claude's is left out, since this
    /// route offers no sign-in of the agent's at all.
    pub fn environment(&self, config_dir: &Path) -> Vec<(String, String)> {
        self.environment_of(
            &config_dir.to_string_lossy(),
            &config_dir.join(AGENT_HOME_FOLDER_NAME).to_string_lossy(),
            &codex_home(config_dir).to_string_lossy(),
        )
    }

    /// [`Self::environment`] for a folder on another host, whose paths this
    /// machine must not join with its own separator (a Windows client
    /// resolving a Linux host's agent).
    pub fn posix_environment(&self, config_dir: &str) -> Vec<(String, String)> {
        let home = format!("{config_dir}/{AGENT_HOME_FOLDER_NAME}");
        let codex_home = format!("{home}/{CODEX_HOME_FOLDER_NAME}");
        self.environment_of(config_dir, &home, &codex_home)
    }

    fn environment_of(
        &self,
        config_dir: &str,
        home: &str,
        codex_home: &str,
    ) -> Vec<(String, String)> {
        let mut environment = vec![("HOME", home.to_string())];
        match self {
            Self::Claude => {
                environment.push(("CLAUDE_CONFIG_DIR", config_dir.to_string()));
                // As for the sandboxed Claude Code CLI: nothing beyond its
                // one host is reachable anyway.
                for name in [
                    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
                    "DISABLE_AUTOUPDATER",
                    "DISABLE_UPDATES",
                    "DISABLE_INSTALLATION_CHECKS",
                    "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
                ] {
                    environment.push((name, "1".to_string()));
                }
                environment.push(("ENABLE_CLAUDEAI_MCP_SERVERS", "false".to_string()));
            }
            Self::Codex => {
                environment.push(("CODEX_HOME", codex_home.to_string()));
                environment.push(("NO_BROWSER", "1".to_string()));
            }
            Self::Gemini => {
                environment.push(("GEMINI_CLI_HOME", home.to_string()));
                environment.push(("NO_BROWSER", "1".to_string()));
            }
            Self::Other { .. } => {}
        }
        environment
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }

    /// The directories, below `config_dir`, that have to exist before the
    /// agent starts: its home, and Codex's `CODEX_HOME`, which it refuses to
    /// use unless it is already there.
    // Only the Unix side of `ensure_agent_config_dir` creates them.
    #[cfg_attr(not(unix), expect(dead_code))]
    fn directories_below(&self, config_dir: &Path) -> Vec<PathBuf> {
        let mut directories = vec![config_dir.join(AGENT_HOME_FOLDER_NAME)];
        if *self == Self::Codex {
            directories.push(codex_home(config_dir));
        }
        directories
    }

    /// [`Self::directories_below`] as paths **relative to the configuration
    /// folder**, POSIX-spelled. ADR-F49 第 2b 段: a Windows client asks the
    /// WSL side to create the folder, and only that side knows where the
    /// distro account's home — and hence the folder — is, so it is given the
    /// names below the folder rather than whole paths this machine would have
    /// to guess.
    pub fn directories_below_relative(&self) -> Vec<String> {
        let mut directories = vec![AGENT_HOME_FOLDER_NAME.to_string()];
        if *self == Self::Codex {
            directories.push(format!("{AGENT_HOME_FOLDER_NAME}/{CODEX_HOME_FOLDER_NAME}"));
        }
        directories
    }
}

const CODEX_HOME_FOLDER_NAME: &str = ".codex";

fn codex_home(config_dir: &Path) -> PathBuf {
    config_dir
        .join(AGENT_HOME_FOLDER_NAME)
        .join(CODEX_HOME_FOLDER_NAME)
}

/// `id` as one path component: kept as it is when it is made of letters,
/// digits, `-`, `_` and `.` only; otherwise those kept, the rest replaced, and
/// the whole told apart by its hash, so that two ids never share a folder.
fn folder_safe_name(id: &str) -> String {
    const MAX_KEPT_CHARACTERS: usize = 64;
    let is_safe =
        |character: char| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.');
    if !id.is_empty() && id.len() <= MAX_KEPT_CHARACTERS && id.chars().all(is_safe) {
        return id.to_string();
    }
    let kept = id
        .chars()
        .take(MAX_KEPT_CHARACTERS)
        .map(|character| if is_safe(character) { character } else { '_' })
        .collect::<String>();
    let digest = Sha256::digest(id.as_bytes());
    let suffix = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{kept}-{suffix}")
}

/// Where the agent's configuration folder for the project `project_key` lies
/// below the account's `home`.
pub fn agent_config_dir(
    home: &Path,
    project_key: &str,
    profile: &SandboxedAgentProfile,
) -> PathBuf {
    let mut path = home.to_path_buf();
    path.extend(sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE.split('/'));
    path.push(project_key);
    path.push(profile.config_folder_name());
    path
}

/// ADR-F53: `CLAUDE_CONFIG_DIR` for the ACP Claude agent started **outside**
/// the sandbox — the host route the user confirmed (決定 19), and a Windows
/// start whose WSL sandbox could not be built.
///
/// A folder of the fork's own, so that credentials the user signed the
/// official Claude Code CLI in with — the account's `~/.claude`, or the
/// sandboxed session's folder — never reach this route, which may only be
/// used with an API key. Nothing of the CLI's folders is read, moved or
/// removed; the agent is pointed elsewhere, that is all.
///
/// One folder for the whole account rather than one per project, unlike the
/// sandboxed route's: outside the sandbox there is no sign-in to keep apart,
/// only the agent's own settings and history.
pub fn claude_acp_host_config_dir() -> PathBuf {
    paths::external_agents_dir().join("claude-acp-config")
}

/// The account's home, below which the sandboxed agents' per-project folders
/// live. A test can put a directory of its own in its place, so that placing
/// an agent never touches the account's real home.
pub fn sandboxed_agents_home() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(home) = test_home::get() {
        return home;
    }
    paths::home_dir().clone()
}

/// The home [`sandboxed_agents_home`] answers with while a test has one.
/// A process-wide value, since the folder is made on a background thread.
#[cfg(any(test, feature = "test-support"))]
pub fn set_sandboxed_agents_home_for_test(home: Option<PathBuf>) {
    test_home::set(home);
}

#[cfg(any(test, feature = "test-support"))]
mod test_home {
    use std::path::PathBuf;
    use std::sync::RwLock;

    static HOME: RwLock<Option<PathBuf>> = RwLock::new(None);

    pub(super) fn get() -> Option<PathBuf> {
        HOME.read().ok()?.clone()
    }

    pub(super) fn set(home: Option<PathBuf>) {
        if let Ok(mut slot) = HOME.write() {
            *slot = home;
        }
    }
}

/// ADR-F49 決定 15: the folder a host it connects to says it placed the agent
/// in, accepted only where it is the folder this client's own policy for the
/// agent names: `<home>/<the projects directory>/<a project key>/<the
/// profile's folder>`. The policy is the client's, so a host cannot put an
/// agent's credentials and hooks where it likes, nor hand one agent another
/// agent's folder (and with it its sign-in).
///
/// The host is a Unix one, so the path is read as one — this client may be
/// Windows.
pub fn accept_remote_config_dir(
    config_dir: &str,
    profile: &SandboxedAgentProfile,
) -> Result<PathBuf> {
    ensure!(
        config_dir.starts_with('/'),
        "the host's folder {config_dir:?} is not an absolute path"
    );
    let segments = config_dir
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    ensure!(
        segments
            .iter()
            .all(|segment| *segment != "." && *segment != ".."),
        "the host's folder {config_dir:?} walks through a relative component"
    );
    let projects = sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE
        .split('/')
        .collect::<Vec<_>>();
    // The account's home, the projects directory, the project's key, the
    // agent's folder.
    let expected_length = 1 + projects.len() + 2;
    let folder_name = profile.config_folder_name();
    let (key, folder) = match segments.as_slice() {
        [.., key, folder] if segments.len() >= expected_length => (*key, *folder),
        _ => anyhow::bail!("the host's folder {config_dir:?} is not an agent's folder"),
    };
    ensure!(
        folder == folder_name,
        "the host placed the agent in {folder:?}, not in its own {folder_name:?}"
    );
    ensure!(
        is_project_key(key),
        "the host placed the agent in {key:?}, which is not a project's directory"
    );
    let below_home = &segments[segments.len() - expected_length + 1..segments.len() - 2];
    ensure!(
        below_home == projects,
        "the host placed the agent outside the sandboxed agents' projects directory: {config_dir:?}"
    );
    Ok(PathBuf::from(config_dir))
}

/// ADR-F49 決定 13: the hosts a host it connects to asked for, kept within
/// this client's policy for the agent — the vendor's own hosts for a
/// registry entry, and for an agent without a profile whatever it asked for,
/// which the IDE-wide allowlist caps anyway.
pub fn accept_remote_hosts(
    profile: &SandboxedAgentProfile,
    requested: AgentSandboxHosts,
) -> AgentSandboxHosts {
    let Some(policy) = profile.hosts() else {
        return requested;
    };
    let hosts = match requested {
        AgentSandboxHosts::Listed(hosts) => hosts
            .into_iter()
            .filter(|host| policy.contains(&host.as_str()))
            .collect(),
        // Not a subset of anything: the agent's policy is its own hosts, not
        // every host the IDE allows.
        AgentSandboxHosts::EditorAllowlist => policy.iter().map(|host| host.to_string()).collect(),
    };
    AgentSandboxHosts::Listed(hosts)
}

/// Creates the agent's configuration folder for the project `project_key`
/// below `home`, and what it needs inside, and returns the folder.
///
/// Each level below `home` is opened without following a link and created
/// owner-only where it is missing; one that is a link, not a directory, not
/// this account's, or writable by every account fails the whole call, because
/// the folder holds the agent's credentials and the hooks it runs. The names
/// every sandbox protects in a root it may write are created in the folder as
/// well: where they exist the remote helper binds them read-only instead of
/// keeping the whole folder from taking new entries, which is where Claude
/// keeps its files.
// Nothing below `home` is created where the folder cannot be made safely, so
// the arguments that say where it would go go unused there.
#[cfg_attr(not(unix), expect(unused_variables))]
pub fn ensure_agent_config_dir(
    home: &Path,
    project_key: &str,
    profile: &SandboxedAgentProfile,
) -> Result<PathBuf> {
    ensure!(
        is_project_key(project_key),
        "プロジェクトの識別子 {project_key:?} が不正です。"
    );
    #[cfg(unix)]
    {
        let config_dir = agent_config_dir(home, project_key, profile);
        let relative = config_dir.strip_prefix(home).map_err(|_| {
            anyhow::anyhow!(
                "{} は {} の下にありません。",
                config_dir.display(),
                home.display()
            )
        })?;
        let folder = no_follow::open_directory_below(home, relative)?;
        for directory in profile.directories_below(&config_dir) {
            let below = directory.strip_prefix(&config_dir).map_err(|_| {
                anyhow::anyhow!("{} は設定フォルダーの下にありません。", directory.display())
            })?;
            no_follow::open_directory_below_fd(&folder, &config_dir, below)?;
        }
        for name in sandbox::WORKTREE_PROTECTED_RELATIVE {
            if name.ends_with(".json") {
                no_follow::create_file_if_missing(&folder, name, &config_dir.join(name))?;
            } else {
                no_follow::open_directory_below_fd(&folder, &config_dir, Path::new(name))?;
            }
        }
        Ok(config_dir)
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "この OS では、エージェントの設定フォルダーをサンドボックス用に作成できません。"
        );
    }
}

#[cfg(unix)]
mod no_follow {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Component, Path, PathBuf};

    use anyhow::{Context as _, Result, bail, ensure};

    /// Opens the directory `relative` below the trusted `base` (the account's
    /// home), one component at a time; see [`open_directory_below_fd`].
    pub(super) fn open_directory_below(base: &Path, relative: &Path) -> Result<OwnedFd> {
        let base_path = CString::new(base.as_os_str().as_bytes())
            .with_context(|| format!("{} に NUL バイトが含まれています。", base.display()))?;
        // The base is where the walk starts from and is trusted, so it is
        // opened following links: a home reached through one is still the
        // account's home.
        // SAFETY: `base_path` is NUL-terminated; a non-creating open takes no
        // mode.
        let raw = unsafe {
            libc::open(
                base_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("{} を開けませんでした。", base.display()));
        }
        // SAFETY: freshly opened by us and owned by nothing else.
        let base_directory = unsafe { OwnedFd::from_raw_fd(raw) };
        open_directory_below_fd(&base_directory, base, relative)
    }

    /// Opens the directory `relative` below the open directory `parent`
    /// (spelled `display`), one component at a time, never following a link
    /// and creating a missing component owner-only. Opening the whole path at
    /// once with `O_NOFOLLOW` would refuse a link only as its last component.
    pub(super) fn open_directory_below_fd(
        parent: &OwnedFd,
        display: &Path,
        relative: &Path,
    ) -> Result<OwnedFd> {
        let mut walked = display.to_path_buf();
        let mut directory: Option<OwnedFd> = None;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                bail!("{} に想定外のパス成分があります。", relative.display());
            };
            walked.push(name);
            let name = CString::new(name.as_bytes())
                .with_context(|| format!("{} に NUL バイトが含まれています。", walked.display()))?;
            let current = directory.as_ref().unwrap_or(parent);
            directory = Some(open_or_create_subdirectory(current, &name, &walked)?);
        }
        directory.context("開くフォルダーがありません。")
    }

    fn open_or_create_subdirectory(
        parent: &OwnedFd,
        name: &CString,
        display: &PathBuf,
    ) -> Result<OwnedFd> {
        let mut created = false;
        let directory = loop {
            // SAFETY: `parent` is open and `name` is NUL-terminated; a
            // non-creating open takes no mode.
            let raw = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw >= 0 {
                // SAFETY: freshly opened by us and owned by nothing else.
                break unsafe { OwnedFd::from_raw_fd(raw) };
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound && !created {
                // SAFETY: `parent` is open and `name` is NUL-terminated.
                if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    let create_error = io::Error::last_os_error();
                    if create_error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(create_error).with_context(|| {
                            format!("{} を作成できませんでした。", display.display())
                        });
                    }
                }
                created = true;
                continue;
            }
            return Err(error).with_context(|| {
                format!(
                    "{} を開けませんでした (リンクではなく実体のフォルダーである必要があります)。",
                    display.display()
                )
            });
        };
        ensure_owned_directory(&directory, display)?;
        Ok(directory)
    }

    /// Creates the empty file `name` in `parent` unless an entry of that name
    /// is there already, without following a link. An entry that is there is
    /// examined without following it either: the agent's folder is read-only
    /// in its sandbox only where the entry is a real file of this account's,
    /// so a link or a directory left there under one of these names is
    /// refused rather than used.
    pub(super) fn create_file_if_missing(
        parent: &OwnedFd,
        name: &str,
        display: &Path,
    ) -> Result<()> {
        let name = CString::new(name)
            .with_context(|| format!("{} に NUL バイトが含まれています。", display.display()))?;
        // SAFETY: `parent` is open, `name` is NUL-terminated, and a creating
        // open is given its mode.
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if raw >= 0 {
            // SAFETY: freshly opened by us and owned by nothing else; dropping
            // it closes it.
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists {
            return ensure_owned_file(parent, &name, display);
        }
        Err(error).with_context(|| format!("{} を作成できませんでした。", display.display()))
    }

    /// The entry `name` already in `parent`, examined without following it:
    /// it has to be a real file of this account's that no other account can
    /// write.
    fn ensure_owned_file(parent: &OwnedFd, name: &CString, display: &Path) -> Result<()> {
        // SAFETY: an all-zero `stat` is a valid value for `fstatat` to
        // overwrite.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `parent` is open, `name` is NUL-terminated and `status` is
        // writable; `AT_SYMLINK_NOFOLLOW` examines the entry itself.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut status,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("{} を確認できませんでした。", display.display()));
        }
        ensure!(
            status.st_mode & libc::S_IFMT == libc::S_IFREG,
            "{} は通常のファイルではありません (リンクであってはいけません)。",
            display.display()
        );
        ensure_owned_by_this_account(&status, display)
    }

    fn ensure_owned_directory(directory: &OwnedFd, display: &Path) -> Result<()> {
        // SAFETY: an all-zero `stat` is a valid value for `fstat` to overwrite.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `directory` is open and `status` is writable.
        if unsafe { libc::fstat(directory.as_raw_fd(), &mut status) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("{} を確認できませんでした。", display.display()));
        }
        ensure!(
            status.st_mode & libc::S_IFMT == libc::S_IFDIR,
            "{} はフォルダーではありません。",
            display.display()
        );
        ensure_owned_by_this_account(&status, display)
    }

    /// The folder holds the agent's credentials and the hooks it runs, so
    /// nothing on the way to it may belong to another account or be writable
    /// by one — its group included, which a shared group would otherwise let
    /// through.
    fn ensure_owned_by_this_account(status: &libc::stat, display: &Path) -> Result<()> {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let own_uid = unsafe { libc::geteuid() };
        ensure!(
            status.st_uid == own_uid,
            "{} は他のアカウントのものです。",
            display.display()
        );
        ensure!(
            status.st_mode & 0o022 == 0,
            "{} は他のアカウントから書き換えられる状態です。",
            display.display()
        );
        Ok(())
    }
}

/// Which hosts a sandboxed agent may reach, on top of the IDE-wide allowlist
/// that caps every sandboxed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentSandboxHosts {
    /// Only these (and only as far as the IDE-wide allowlist allows them).
    Listed(Vec<String>),
    /// Whatever the IDE-wide allowlist allows: an agent with no profile.
    EditorAllowlist,
}

/// Where an agent's sandbox part stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentSandboxState {
    /// Still to be placed in a project, under this profile.
    Unresolved { profile: SandboxedAgentProfile },
    /// Placed: its folder exists and its environment points into it.
    Resolved {
        config_dir: PathBuf,
        hosts: AgentSandboxHosts,
    },
    /// The host it runs on tried to place it and could not, for this reason.
    Unavailable { reason: String },
}

/// ADR-F49 第 2a 段: an agent command's part in the sandbox, next to the
/// command itself.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSandboxCommand {
    /// The agent's own environment: its settings', its distribution's and
    /// Zed's additions — nothing of the host's shell or direnv environment.
    pub env: HashMap<String, String>,
    /// What the agent runs from inside Zed's data directory (its
    /// installation, Zed's node), which the sandbox shows back read-only.
    pub readable_paths: Vec<PathBuf>,
    pub state: AgentSandboxState,
    /// ADR-F49 第 2b 段: `Some` when the command — its program, arguments,
    /// environment, `readable_paths` and, once placed, `config_dir` — is a
    /// Linux command inside this WSL distro, which a Windows client starts
    /// through the distro's sandbox helper.
    pub wsl: Option<WslAgentRuntime>,
}

/// ADR-F49 第 2b 段: the WSL distro an agent's Linux runtime lives in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslAgentRuntime {
    /// `None`: the default distro.
    pub distro: Option<String>,
}

/// ADR-F49 第 2b 段 (decision 23): the WSL distro a Windows project's agent
/// runs in — the one a `\\wsl.localhost\<distro>\` root names, else the
/// default one (`None`) — and an error for folders spread over several
/// distros. Elsewhere than Windows: `None`.
#[cfg_attr(not(target_os = "windows"), expect(unused_variables))]
pub fn wsl_distro_for_roots(roots: &[PathBuf]) -> Result<Option<String>> {
    #[cfg(target_os = "windows")]
    {
        let mut distro: Option<String> = None;
        for root in roots {
            let Some(root_distro) = sandbox::wsl_distro_for_path(root) else {
                continue;
            };
            match &distro {
                Some(existing) if !existing.eq_ignore_ascii_case(&root_distro) => anyhow::bail!(
                    "複数の WSL ディストリビューション ({existing}、{root_distro}) にまたがるフォルダーでは起動できません。"
                ),
                _ => distro = Some(root_distro),
            }
        }
        Ok(distro)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(None)
    }
}

/// ADR-F50: what a session in a WSL distro's sandbox may write, spelled as
/// worktrees are — the project's folders, and its configuration folder through
/// the distro's share, since that folder lives on the distro's own filesystem
/// and a worktree can hold it (the WSL home opened as a project). `distro` is
/// the distro's registered name; without one (the default distro, whose name
/// Zed does not know) the folder has no Windows spelling and is left out, so a
/// worktree holding it is not paused for this session.
///
/// Shared by the sandboxed Claude Code CLI (第 1 段) and an external agent
/// started in the WSL sandbox (第 2b 段), so both pause the same worktrees.
pub fn wsl_session_write_scope(
    roots: &[PathBuf],
    config_dir: &str,
    distro: Option<&str>,
) -> Vec<PathBuf> {
    let mapper = util::path_mapping::WslPathMapper {
        distro: distro.unwrap_or_default().to_string(),
    };
    let mut scope = roots.to_vec();
    match util::path_mapping::PathMapper::to_host(&mapper, config_dir) {
        Some(host_path) => scope.push(host_path),
        None => log::warn!(
            "[agent-sandbox] {config_dir} has no Windows path (distro {distro:?}), so a \
             worktree holding it is not paused for this session"
        ),
    }
    scope
}

impl std::fmt::Debug for AgentSandboxCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env = self
            .env
            .iter()
            .map(|(name, value)| {
                let value = if util::redact::should_redact(name) {
                    "[REDACTED]"
                } else {
                    value.as_str()
                };
                (name, value)
            })
            .collect::<Vec<_>>();
        formatter
            .debug_struct("AgentSandboxCommand")
            .field("env", &env)
            .field("readable_paths", &self.readable_paths)
            .field("state", &self.state)
            .field("wsl", &self.wsl)
            .finish()
    }
}

impl AgentSandboxCommand {
    pub fn new(
        profile: SandboxedAgentProfile,
        env: HashMap<String, String>,
        readable_paths: Vec<PathBuf>,
    ) -> Self {
        Self {
            env,
            readable_paths,
            state: AgentSandboxState::Unresolved { profile },
            wsl: None,
        }
    }

    /// Places the agent in the project whose folders are `canonical_roots`:
    /// creates its configuration folder below `home` (blocking; call it off
    /// the main thread) and lays its profile's environment over its own,
    /// since that environment is what confines it. Already placed, it is
    /// returned as it is.
    pub fn resolve_for_project(self, home: &Path, canonical_roots: &[PathBuf]) -> Result<Self> {
        let AgentSandboxState::Unresolved { profile } = &self.state else {
            return Ok(self);
        };
        ensure!(
            !canonical_roots.is_empty(),
            "フォルダーを開いていないプロジェクトでは、エージェントをサンドボックスで起動できません。"
        );
        let key = project_key(canonical_roots);
        let config_dir = ensure_agent_config_dir(home, &key, profile)?;
        let hosts = match profile.hosts() {
            Some(hosts) => {
                AgentSandboxHosts::Listed(hosts.iter().map(|host| host.to_string()).collect())
            }
            None => AgentSandboxHosts::EditorAllowlist,
        };
        let mut env = self.env;
        env.extend(profile.environment(&config_dir));
        Ok(Self {
            env,
            readable_paths: self.readable_paths,
            state: AgentSandboxState::Resolved { config_dir, hosts },
            wsl: self.wsl,
        })
    }

    pub fn config_dir(&self) -> Option<&Path> {
        match &self.state {
            AgentSandboxState::Resolved { config_dir, .. } => Some(config_dir),
            AgentSandboxState::Unresolved { .. } | AgentSandboxState::Unavailable { .. } => None,
        }
    }
}

/// The read-only locations for an agent run from `paths` inside Zed's data
/// directory, `data_dir`: those of them there, each once. Anything outside it
/// is readable in the sandbox anyway, and the sandbox refuses to show back
/// anything else of it.
pub fn readable_paths_in_data_dir(data_dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut readable = Vec::new();
    for path in paths {
        if path.starts_with(data_dir) && !readable.contains(path) {
            readable.push(path.clone());
        }
    }
    readable
}
