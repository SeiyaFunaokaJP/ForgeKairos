//! ADR-F42: host programs that a project's own settings would launch — MCP
//! context servers, `lsp.<name>.binary` and `formatter.external` — wait for an
//! approval bound to the exact definition. Anything that can write the
//! worktree (a sandboxed agent command included) can write
//! `.zed/settings.json`, so the approval follows the file's content rather
//! than the folder's trust. Worktree trust and `session.trust_all_worktrees`
//! deliberately play no part here.

use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Task,
};
use itertools::Itertools as _;
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use settings::{
    BinarySettings, Formatter, FormatterList, ProjectSettingsContent, SettingsFile, SettingsStore,
};
use sha2::{Digest as _, Sha256};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use worktree::{Snapshot, Worktree};

use crate::remote_launch_approvals::{
    FORMATTER_ANSWER_TIMEOUT, LANGUAGE_SERVER_ANSWER_TIMEOUT, LaunchPlace, RemoteAnswer,
    RemoteLaunchApprovals,
};

const DB_NAMESPACE: &str = "project_launch_approvals";

const DEFAULT_LANGUAGE_SERVER_PROGRAM: &str = "(言語サーバーの既定の実行ファイル)";

/// The error for a decision about a definition that has changed since it was
/// shown, so that only what the user saw gets decided.
pub const CHANGED_SINCE_SHOWN_MESSAGE: &str = "内容が変わりました。もう一度確認してください";

/// Programs that find code in their working directory on their own — module
/// and package lookup, `./node_modules/.bin`, scripts named without their
/// extension — which no approval of the command line can pin down.
const WORKING_DIRECTORY_CODE_RUNNERS: &[&str] = &[
    "node",
    "python",
    "python3",
    "py",
    "uv",
    "uvx",
    "npx",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "ruby",
    "perl",
    "php",
    "bash",
    "sh",
    "zsh",
    "pwsh",
    "powershell",
    "cmd",
];

/// How an interpreter or runner takes its script, so that the script can be
/// told apart from its options and their values.
struct ScriptConvention {
    /// Options whose value is the next argument, which is not the script.
    options_with_a_value: &'static [&'static str],
    /// Options after which the code comes from the command line itself
    /// (`python -c`, `node -e`), so there is no script.
    inline_code_options: &'static [&'static str],
    /// Options after which a module is looked up (`python -m`), so there is
    /// no script either.
    module_options: &'static [&'static str],
    /// Options whose value is the script (PowerShell's `-File`).
    script_options: &'static [&'static str],
    /// Subcommands that come before the script: `uv run server.py`.
    subcommands: &'static [&'static str],
    /// Subcommands that run code given inline: `deno eval`.
    inline_subcommands: &'static [&'static str],
    /// The extensions of what it runs as a script, checked after a
    /// subcommand, or always for a runner with many options of its own: a
    /// config file taken for the script because an option is missing from
    /// these tables is then never bound.
    entrypoint_extensions: &'static [&'static str],
    always_checks_extension: bool,
    ignores_case: bool,
    /// `cmd`'s options start with `/`.
    slash_options: bool,
}

const NO_CONVENTION: ScriptConvention = ScriptConvention {
    options_with_a_value: &[],
    inline_code_options: &[],
    module_options: &[],
    script_options: &[],
    subcommands: &[],
    inline_subcommands: &[],
    entrypoint_extensions: &[],
    always_checks_extension: false,
    ignores_case: false,
    slash_options: false,
};

const JAVASCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"];

const NODE: ScriptConvention = ScriptConvention {
    options_with_a_value: &[
        "-r",
        "--require",
        "--import",
        "--loader",
        "--experimental-loader",
        "-C",
        "--conditions",
        "--env-file",
        "--env-file-if-exists",
        "--inspect-port",
        "--title",
        "--input-type",
        "--watch-path",
        "--disable-warning",
        "--redirect-warnings",
        "--openssl-config",
        "--icu-data-dir",
    ],
    // `--check` only checks the syntax: nothing runs.
    inline_code_options: &["-e", "--eval", "-p", "--print", "-c", "--check"],
    ..NO_CONVENTION
};

const PYTHON: ScriptConvention = ScriptConvention {
    options_with_a_value: &["-W", "-X", "--check-hash-based-pycs"],
    inline_code_options: &["-c"],
    module_options: &["-m"],
    ..NO_CONVENTION
};

const UV: ScriptConvention = ScriptConvention {
    options_with_a_value: &[
        "--with",
        "--with-editable",
        "--with-requirements",
        "--from",
        "--python",
        "-p",
        "--directory",
        "--project",
        "--package",
        "--env-file",
        "--index",
        "--index-url",
        "--extra-index-url",
        "--default-index",
        "--find-links",
        "-f",
        "--extra",
        "--group",
        "--only-group",
        "--config-file",
        "--cache-dir",
        "-C",
        "--config-setting",
        "--exclude-newer",
        "--resolution",
        "--prerelease",
        "--python-platform",
        "--color",
    ],
    subcommands: &["run"],
    entrypoint_extensions: &["py", "pyw"],
    ..NO_CONVENTION
};

/// `uvx` runs a tool by its package name, never a file of the project.
const UVX: ScriptConvention = ScriptConvention {
    always_checks_extension: true,
    ..UV
};

const DENO: ScriptConvention = ScriptConvention {
    options_with_a_value: &[
        "-c",
        "--config",
        "--import-map",
        "--lock",
        "--cert",
        "--v8-flags",
        "--seed",
        "--location",
        "--ext",
        "--port",
        "--host",
        "--preload",
    ],
    subcommands: &["run", "serve"],
    inline_subcommands: &["eval", "repl"],
    entrypoint_extensions: JAVASCRIPT_EXTENSIONS,
    always_checks_extension: true,
    ..NO_CONVENTION
};

const BUN: ScriptConvention = ScriptConvention {
    // `--preload` names a file it loads, which is warned about as any other
    // file the command line names.
    options_with_a_value: &[
        "-c",
        "--config",
        "--tsconfig-override",
        "--preload",
        "-r",
        "--cwd",
        "--env-file",
        "--main-fields",
        "--define",
        "-d",
        "--loader",
        "-l",
        "--port",
        "--conditions",
        "--extension-order",
        "--filter",
        "-F",
        "--jsx-factory",
        "--jsx-fragment",
        "--jsx-import-source",
        "--jsx-runtime",
    ],
    inline_code_options: &["-e", "--eval", "-p", "--print"],
    subcommands: &["run"],
    entrypoint_extensions: JAVASCRIPT_EXTENSIONS,
    always_checks_extension: true,
    ..NO_CONVENTION
};

/// npm, pnpm, yarn and npx run package scripts and packages by name.
const NPM: ScriptConvention = ScriptConvention {
    options_with_a_value: &[
        "--prefix",
        "-w",
        "--workspace",
        "-p",
        "--package",
        "--cache",
        "--registry",
        "--userconfig",
        "--globalconfig",
        "--loglevel",
        "--script-shell",
        "--dir",
        "-C",
        "--filter",
        "-F",
        "--cwd",
    ],
    inline_code_options: &["-c", "--call"],
    subcommands: &["run", "run-script", "exec", "x", "dlx"],
    entrypoint_extensions: &["js", "mjs", "cjs"],
    always_checks_extension: true,
    ..NO_CONVENTION
};

const POWERSHELL: ScriptConvention = ScriptConvention {
    options_with_a_value: &[
        "-executionpolicy",
        "-ep",
        "-ex",
        "-workingdirectory",
        "-wd",
        "-windowstyle",
        "-w",
        "-configurationname",
        "-config",
        "-outputformat",
        "-of",
        "-o",
        "-inputformat",
        "-if",
        "-in",
        "-settingsfile",
        "-settings",
        "-custompipename",
        "-psconsolefile",
    ],
    inline_code_options: &[
        "-command",
        "-c",
        "-encodedcommand",
        "-e",
        "-ec",
        "-commandwithargs",
        "-cwa",
    ],
    script_options: &["-file", "-f"],
    ignores_case: true,
    ..NO_CONVENTION
};

const CMD: ScriptConvention = ScriptConvention {
    inline_code_options: &["/c", "/k", "/r"],
    ignores_case: true,
    slash_options: true,
    ..NO_CONVENTION
};

const SHELL: ScriptConvention = ScriptConvention {
    options_with_a_value: &["-o", "-O", "--init-file", "--rcfile"],
    inline_code_options: &["-c"],
    ..NO_CONVENTION
};

const RUBY: ScriptConvention = ScriptConvention {
    options_with_a_value: &["-I", "-r", "-C", "-E", "--encoding"],
    inline_code_options: &["-e"],
    ..NO_CONVENTION
};

const PERL: ScriptConvention = ScriptConvention {
    options_with_a_value: &["-I"],
    inline_code_options: &["-e", "-E"],
    ..NO_CONVENTION
};

const PHP: ScriptConvention = ScriptConvention {
    options_with_a_value: &["-c", "-d", "-z"],
    inline_code_options: &["-r"],
    script_options: &["-f"],
    ..NO_CONVENTION
};

/// What no interpreter or runner runs as its script: a config file or
/// lockfile an unknown option took is not bound in its place.
const NOT_ENTRYPOINT_EXTENSIONS: &[&str] = &[
    "json", "jsonc", "json5", "lock", "lockb", "toml", "yaml", "yml", "ini", "cfg", "conf", "env",
    "txt", "md", "map", "log", "xml", "csv",
];

/// A program or script larger than this is not read for the key, so an
/// approval of it lasts only for the session.
const MAX_BOUND_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Environment variables whose value decides which code a program loads.
/// Their values are shown for approval; every other value stays hidden.
const CODE_LOADING_ENVIRONMENT_VARIABLES: &[&str] = &[
    "PATH",
    "PATHEXT",
    "COMSPEC",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSTARTUP",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "GCONV_PATH",
    "RUBYOPT",
    "RUBYLIB",
    "PERL5OPT",
    "PERL5LIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "CLASSPATH",
    "BASH_ENV",
    "ENV",
    "PS4",
];
const CODE_LOADING_ENVIRONMENT_PREFIXES: &[&str] = &["DYLD_", "GIT_"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    ContextServer,
    LanguageServerBinary,
    ExternalFormatter,
}

/// The machine that owns the worktree a definition was read from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LaunchHost {
    ThisMachine,
    Remote {
        identity: SharedString,
        display_name: SharedString,
    },
}

impl LaunchHost {
    pub fn from_remote_connection(options: &RemoteConnectionOptions) -> Self {
        let identity = match options {
            RemoteConnectionOptions::Ssh(ssh) => {
                let mut identity = format!(
                    "ssh://{}{}{}",
                    ssh.username
                        .as_deref()
                        .map(|user| format!("{user}@"))
                        .unwrap_or_default(),
                    ssh.host.to_bracketed_string(),
                    ssh.port.map(|port| format!(":{port}")).unwrap_or_default(),
                );
                // The same name can lead to another machine through a jump
                // host, a proxy command or another configuration file.
                if let Some(arguments) = ssh.args.as_ref().filter(|arguments| !arguments.is_empty())
                {
                    // Only a digest: the identity is stored with decisions,
                    // and a proxy command can hold a secret.
                    let digest = Sha256::digest(
                        serde_json::Value::from(arguments.clone())
                            .to_string()
                            .as_bytes(),
                    );
                    identity.push_str(&format!(" args-sha256:{digest:x}"));
                }
                identity
            }
            RemoteConnectionOptions::Wsl(wsl) => format!(
                "wsl://{}{}",
                wsl.user
                    .as_deref()
                    .map(|user| format!("{user}@"))
                    .unwrap_or_default(),
                wsl.distro_name,
            ),
            RemoteConnectionOptions::Docker(docker) => {
                format!("docker://{}", docker.container_id)
            }
            #[cfg(feature = "test-support")]
            RemoteConnectionOptions::Mock(mock) => format!("mock://{}", mock.id),
        };
        Self::Remote {
            identity: identity.into(),
            display_name: options.display_name().into(),
        }
    }

    pub(crate) fn identity(&self) -> Option<&str> {
        match self {
            Self::ThisMachine => None,
            Self::Remote { identity, .. } => Some(identity.as_ref()),
        }
    }
}

/// Where the launched program runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionHost {
    ThisMachine,
    Remote { display_name: SharedString },
}

impl ExecutionHost {
    pub(crate) fn of(host: &LaunchHost) -> Self {
        match host {
            LaunchHost::ThisMachine => Self::ThisMachine,
            LaunchHost::Remote { display_name, .. } => Self::Remote {
                display_name: display_name.clone(),
            },
        }
    }
}

/// Where the launched definition comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LaunchDefinitionFile {
    /// A `.zed/settings.json`, as a path on the host that owns the worktree.
    Worktree(SharedString),
    /// The remote host's own settings, which commands run over there can
    /// rewrite as well.
    RemoteServerSettings,
    /// A command the remote host resolved on its own (an extension's server
    /// running there), which this machine cannot predict.
    ResolvedOnRemoteHost,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LaunchTarget {
    Command {
        program: SharedString,
        arguments: Vec<SharedString>,
    },
    Url(SharedString),
}

impl LaunchTarget {
    /// The full command line or URL, as it would run: paths read exactly as
    /// they are, backslashes included. Characters that would not show up as
    /// themselves are written as `<U+XXXX>`, and a part with spaces or quotes
    /// is wrapped in quotes as it is (the parts are also shown one by one,
    /// which is unambiguous).
    pub fn display(&self) -> SharedString {
        match self {
            Self::Command { program, arguments } => std::iter::once(program)
                .chain(arguments)
                .map(|part| quote_command_line_part(part.as_ref()))
                .join(" ")
                .into(),
            Self::Url(url) => escape_for_display(url).into_owned().into(),
        }
    }
}

/// Writes characters that would not show up as themselves — controls,
/// zero-width and other format characters, direction overrides and isolates,
/// blank fillers — as `<U+XXXX>`, so that text shown for approval cannot hide
/// or reorder what actually runs. Everything else, backslashes included, is
/// left as it is.
pub fn escape_for_display(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_deceptive_character) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        push_display_character(character, &mut escaped);
    }
    Cow::Owned(escaped)
}

fn is_deceptive_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{08E2}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

fn push_display_character(character: char, output: &mut String) {
    if is_deceptive_character(character) {
        output.push_str(&format!("<U+{:04X}>", u32::from(character)));
    } else {
        output.push(character);
    }
}

fn quote_command_line_part(part: &str) -> Cow<'_, str> {
    let shown = escape_for_display(part);
    if part.is_empty()
        || part.contains(|character: char| character.is_whitespace() || character == '"')
    {
        Cow::Owned(format!("\"{shown}\""))
    } else {
        shown
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LaunchApprovalKey(pub(crate) SharedString);

impl LaunchApprovalKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(key: &str) -> Self {
        Self(key.to_string().into())
    }
}

impl fmt::Display for LaunchApprovalKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDecision {
    Approved,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchApprovalState {
    /// Neither approved nor denied yet; nothing is launched.
    Pending,
    Approved,
    /// Nothing is launched and no further notification is shown.
    Denied,
}

/// What a launch may run that its approval does not pin down, so the
/// approval UI has to warn about it. Only the program and an interpreter's
/// script are bound by their contents; everything else a command line names
/// can change afterwards without anyone being asked again.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UnboundWorktreeContent {
    /// The program starts in a worktree directory, and is not itself a
    /// worktree file the key covers (an interpreter or runner also needs its
    /// script covered): what it picks up from there (`-m module`,
    /// `./node_modules/.bin`, `server` for `server.js`, a config file) is not
    /// part of the approval.
    WorkingDirectory {
        program: SharedString,
        working_directory: SharedString,
    },
    /// A worktree file the command line or its environment names besides the
    /// program and its script (`--config ./x.toml`,
    /// `NODE_OPTIONS=--require=./hook.js`).
    File(SharedString),
    /// A worktree directory named by the command line or its environment.
    Directory(SharedString),
    /// A worktree file an `@file` argument reads more arguments from.
    ArgumentFile(SharedString),
    /// A wrapper that picks what runs on its own terms — `wsl.exe`,
    /// `direnv exec`, `mise exec`, `env -S` or `env -C` — so nothing behind
    /// it is pinned down.
    Wrapper(SharedString),
    /// A path in a remote worktree, whose contents cannot be read from here.
    /// When it is the program or its script, the approval lasts only for this
    /// session.
    RemoteFile(SharedString),
    /// A program or script in a worktree that could not be read — not a
    /// regular file, larger than the limit, or unreadable — or a worktree
    /// whose identity could not be read, so that paths into it could only be
    /// compared by their spelling. The approval lasts only for this session.
    Unreadable(SharedString),
}

/// What the approval UI shows for one definition. Values of environment
/// variables and headers are left out on purpose, except for the variables
/// that decide which code runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchApprovalDetails {
    pub key: LaunchApprovalKey,
    pub kind: LaunchKind,
    pub name: SharedString,
    pub target: LaunchTarget,
    pub environment_variable_names: Vec<SharedString>,
    pub header_names: Vec<SharedString>,
    /// Name and value of every environment variable that decides which code
    /// the program loads (`PATH`, `NODE_OPTIONS`, `PYTHONPATH`, ...).
    pub code_loading_environment: Vec<(SharedString, SharedString)>,
    pub execution_host: ExecutionHost,
    pub host: LaunchHost,
    pub definition_file: LaunchDefinitionFile,
    pub worktree_abs_path: Arc<Path>,
    /// The worktree files the key is bound to by their contents: the program
    /// and, for an interpreter or runner, its script. The UI should still
    /// warn: what the command line shows says nothing about what such a file
    /// does.
    pub referenced_worktree_files: Vec<SharedString>,
    /// What else may run that the key does not cover.
    pub unbound_worktree_content: Vec<UnboundWorktreeContent>,
    /// Whether an approval is forgotten when Zed quits, because what runs
    /// could not be pinned down: a program or script in a remote worktree or
    /// one that could not be read, or a worktree whose identity could not be
    /// read.
    pub approval_is_session_only: bool,
    /// Whether the project's entry replaces one the user defined under the
    /// same name.
    pub replaces_user_entry: bool,
    pub state: LaunchApprovalState,
    /// Why the stored decisions could not be read. The launch stays blocked.
    pub store_error: Option<SharedString>,
}

impl LaunchApprovalDetails {
    /// Whether the launch runs code from a worktree, bound by the key or not,
    /// which the approval UI warns about.
    pub fn references_worktree_file(&self) -> bool {
        !self.referenced_worktree_files.is_empty() || !self.unbound_worktree_content.is_empty()
    }

    /// A pending launch of `target` from `/project/.zed/settings.json` on this
    /// machine; adjust the public fields for other cases.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(key: &str, kind: LaunchKind, name: &str, target: LaunchTarget) -> Self {
        Self {
            key: LaunchApprovalKey::for_tests(key),
            kind,
            name: name.to_string().into(),
            target,
            environment_variable_names: Vec::new(),
            header_names: Vec::new(),
            code_loading_environment: Vec::new(),
            execution_host: ExecutionHost::ThisMachine,
            host: LaunchHost::ThisMachine,
            definition_file: LaunchDefinitionFile::Worktree("/project/.zed/settings.json".into()),
            worktree_abs_path: Path::new("/project").into(),
            referenced_worktree_files: Vec::new(),
            unbound_worktree_content: Vec::new(),
            approval_is_session_only: false,
            replaces_user_entry: false,
            state: LaunchApprovalState::Pending,
            store_error: None,
        }
    }

    fn summary(&self) -> String {
        let mut summary = self.target.display().to_string();
        if !self.environment_variable_names.is_empty() {
            summary.push_str(&format!(
                " (環境変数: {})",
                self.environment_variable_names.iter().join(", ")
            ));
        }
        if !self.code_loading_environment.is_empty() {
            summary.push_str(&format!(
                " ({})",
                self.code_loading_environment
                    .iter()
                    .map(|(name, value)| format!("{name}={}", escape_for_display(value)))
                    .join(", ")
            ));
        }
        if !self.header_names.is_empty() {
            summary.push_str(&format!(
                " (ヘッダー: {})",
                self.header_names.iter().join(", ")
            ));
        }
        summary
    }
}

/// A decision, as kept in the database (or, for a session-only approval,
/// only in memory).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchDecisionRecord {
    pub key: String,
    pub kind: LaunchKind,
    pub name: String,
    pub worktree: String,
    pub host: Option<String>,
    pub summary: String,
    pub decision: LaunchDecision,
    /// Seconds since the Unix epoch.
    pub timestamp: u64,
    #[serde(default)]
    pub session_only: bool,
}

impl LaunchDecisionRecord {
    fn new(details: &LaunchApprovalDetails, decision: LaunchDecision, session_only: bool) -> Self {
        Self {
            key: details.key.as_str().to_string(),
            kind: details.kind,
            name: details.name.to_string(),
            worktree: details.worktree_abs_path.to_string_lossy().into_owned(),
            host: details.host.identity().map(str::to_string),
            summary: details.summary(),
            decision,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
            session_only,
        }
    }
}

/// A definition gathered from settings, before the worktree content it runs
/// is looked up and its key is computed.
#[derive(Clone)]
pub(crate) struct LaunchRequest {
    pub(crate) kind: LaunchKind,
    pub(crate) name: SharedString,
    pub(crate) definition: serde_json::Value,
    pub(crate) target: LaunchTarget,
    pub(crate) environment_variable_names: Vec<SharedString>,
    pub(crate) header_names: Vec<SharedString>,
    pub(crate) code_loading_environment: Vec<(SharedString, SharedString)>,
    pub(crate) execution_host: ExecutionHost,
    pub(crate) host: LaunchHost,
    pub(crate) definition_file: LaunchDefinitionFile,
    pub(crate) worktree_abs_path: Arc<Path>,
    pub(crate) replaces_user_entry: bool,
    pub(crate) scan: CommandLineScan,
}

impl LaunchRequest {
    pub(crate) fn language_server_binary(
        server_name: &str,
        binary: &BinarySettings,
        definition: UntrustedDefinition,
        worktree_abs_path: Arc<Path>,
        worktrees: ScannedWorktrees,
    ) -> Self {
        let arguments = binary.arguments.clone().unwrap_or_default();
        let environment = binary.env.clone().unwrap_or_default();
        let scan = CommandLineScan::new(
            binary.path.as_deref(),
            &arguments,
            environment
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            Some(&*worktree_abs_path),
            worktrees,
        )
        .resolving_program_like_language_servers();
        Self {
            kind: LaunchKind::LanguageServerBinary,
            name: server_name.to_string().into(),
            definition: serde_json::json!({
                "path": binary.path,
                "arguments": binary.arguments,
                "env": binary.env,
            }),
            target: LaunchTarget::Command {
                program: binary
                    .path
                    .clone()
                    .unwrap_or_else(|| DEFAULT_LANGUAGE_SERVER_PROGRAM.to_string())
                    .into(),
                arguments: arguments.into_iter().map(SharedString::from).collect(),
            },
            environment_variable_names: environment
                .keys()
                .map(|name| name.clone().into())
                .collect(),
            header_names: Vec::new(),
            code_loading_environment: code_loading_environment(
                environment
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            ),
            execution_host: ExecutionHost::ThisMachine,
            host: LaunchHost::ThisMachine,
            definition_file: definition.file,
            worktree_abs_path,
            replaces_user_entry: definition.replaces_user_entry,
            scan,
        }
    }

    pub(crate) fn external_formatter(
        command: &str,
        arguments: Option<&[String]>,
        definition: UntrustedDefinition,
        worktree_abs_path: Arc<Path>,
        working_directory: &Path,
        worktrees: ScannedWorktrees,
    ) -> Self {
        let argument_list = arguments.map(<[String]>::to_vec).unwrap_or_default();
        let scan = CommandLineScan::new(
            Some(command),
            &argument_list,
            std::iter::empty(),
            Some(working_directory),
            worktrees,
        );
        Self {
            kind: LaunchKind::ExternalFormatter,
            name: command.to_string().into(),
            definition: serde_json::json!({
                "command": command,
                "arguments": arguments,
            }),
            target: LaunchTarget::Command {
                program: command.to_string().into(),
                arguments: argument_list.into_iter().map(SharedString::from).collect(),
            },
            environment_variable_names: Vec::new(),
            header_names: Vec::new(),
            code_loading_environment: Vec::new(),
            execution_host: ExecutionHost::ThisMachine,
            host: LaunchHost::ThisMachine,
            definition_file: definition.file,
            worktree_abs_path,
            replaces_user_entry: definition.replaces_user_entry,
            scan,
        }
    }

    /// Looks up the worktree content the command line runs and computes the
    /// key. `state` is left `Pending` for
    /// [`ProjectLaunchApprovals::request_launch`] to fill in.
    pub(crate) async fn resolve(self, fs: Option<Arc<dyn Fs>>) -> LaunchApprovalDetails {
        let scanned = self.scan.run(fs.as_ref()).await;
        // What else the command line names stays out of the key, so that a
        // log file appearing (`--log ./x.log`) does not take the approval away.
        let key_material = serde_json::json!({
            "kind": self.kind,
            "name": self.name.as_ref(),
            "definition": self.definition,
            "worktree": self.worktree_abs_path.to_string_lossy(),
            "working_directory": self
                .scan
                .working_directory
                .as_deref()
                .map(|directory| directory.to_string_lossy()),
            "host": self.host.identity(),
            "bound_files": scanned
                .bound_files
                .iter()
                .map(|(path, fingerprint)| {
                    serde_json::json!({ "path": path.as_ref(), "fingerprint": fingerprint })
                })
                .collect::<Vec<_>>(),
            "unbound_programs": scanned.unbound_program_key_parts,
        });
        let digest = Sha256::digest(canonical_json(&key_material).as_bytes());
        let approval_is_session_only = scanned.session_only;
        LaunchApprovalDetails {
            key: LaunchApprovalKey(format!("{digest:x}").into()),
            kind: self.kind,
            name: self.name,
            target: self.target,
            // Code-loading variables are shown with their values instead.
            environment_variable_names: sorted(
                self.environment_variable_names
                    .into_iter()
                    .filter(|name| {
                        !self
                            .code_loading_environment
                            .iter()
                            .any(|(code_loading_name, _)| code_loading_name == name)
                    })
                    .collect(),
            ),
            header_names: sorted(self.header_names),
            code_loading_environment: self.code_loading_environment,
            execution_host: self.execution_host,
            host: self.host,
            definition_file: self.definition_file,
            worktree_abs_path: self.worktree_abs_path,
            referenced_worktree_files: scanned
                .bound_files
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
            unbound_worktree_content: scanned.unbound,
            approval_is_session_only,
            replaces_user_entry: self.replaces_user_entry,
            state: LaunchApprovalState::Pending,
            store_error: None,
        }
    }
}

fn sorted(mut names: Vec<SharedString>) -> Vec<SharedString> {
    names.sort();
    names
}

/// Serializes with object keys sorted, so that equal definitions hash
/// equally no matter how their maps were ordered.
pub(crate) fn canonical_json(value: &serde_json::Value) -> String {
    let mut output = String::new();
    write_canonical_json(value, &mut output);
    output
}

fn write_canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            output.push('{');
            for (index, (key, value)) in map
                .iter()
                .sorted_by(|(left, _), (right, _)| left.cmp(right))
                .enumerate()
            {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::Value::String(key.clone()).to_string());
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
        serde_json::Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(item, output);
            }
            output.push(']');
        }
        other => output.push_str(&other.to_string()),
    }
}

/// The name and value of each variable that decides which code runs.
pub(crate) fn code_loading_environment<'a>(
    environment: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<(SharedString, SharedString)> {
    environment
        .into_iter()
        .filter(|(name, _)| is_code_loading_variable(name))
        .map(|(name, value)| {
            (
                SharedString::from(name.to_string()),
                SharedString::from(value.to_string()),
            )
        })
        .sorted()
        .collect()
}

fn is_code_loading_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    CODE_LOADING_ENVIRONMENT_VARIABLES.contains(&name.as_str())
        || CODE_LOADING_ENVIRONMENT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn runs_code_from_working_directory(program: &str) -> bool {
    let stem = program_stem(program);
    stem.starts_with("python") || WORKING_DIRECTORY_CODE_RUNNERS.contains(&stem.as_str())
}

/// The lowercase file name of `program`, without an executable extension.
fn program_stem(program: &str) -> String {
    let file_name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    [".exe", ".cmd", ".bat", ".com", ".ps1"]
        .iter()
        .find_map(|extension| file_name.strip_suffix(*extension))
        .unwrap_or(&file_name)
        .to_string()
}

/// A program named without a directory or by its full path, as a system
/// program is: a relative path names a file of its own.
fn is_bare_or_absolute(program: &str) -> bool {
    !program.contains(['/', '\\']) || program.starts_with('/') || Path::new(program).is_absolute()
}

/// The worktrees a command line may run content from.
#[derive(Clone, Default)]
pub(crate) struct ScannedWorktrees {
    local_roots: Vec<PathBuf>,
    remote: Vec<Snapshot>,
}

impl ScannedWorktrees {
    pub(crate) fn of<'a>(worktrees: impl IntoIterator<Item = &'a Worktree>) -> Self {
        let mut scanned = Self::default();
        for worktree in worktrees {
            if worktree.as_local().is_some() {
                scanned.local_roots.push(worktree.abs_path().to_path_buf());
            } else {
                scanned.remote.push(worktree.snapshot());
            }
        }
        scanned
    }

    fn contains(&self, path: &Path) -> bool {
        self.local_roots.iter().any(|root| path.starts_with(root))
            || self.remote.iter().any(|snapshot| {
                snapshot
                    .path_style()
                    .strip_prefix(path, snapshot.abs_path().as_ref())
                    .is_some()
            })
    }
}

/// What a command line could run from the worktrees. Only the program and
/// an interpreter's script are bound by their contents; everything else it
/// names (its other arguments, a `--flag=value` also by its value, `@file`
/// arguments, environment values) only gets a warning, so that a file
/// appearing or changing there does not take the approval away.
#[derive(Clone)]
pub(crate) struct CommandLineScan {
    /// `None` for a launch that runs no command line (a URL).
    command: Option<CommandLineParts>,
    environment_candidates: Vec<String>,
    working_directory: Option<PathBuf>,
    worktrees: ScannedWorktrees,
    /// A language server binary named without a directory is also looked up
    /// in the worktree root, and `~` is the home directory.
    program_resolves_like_language_servers: bool,
}

impl CommandLineScan {
    pub(crate) fn new<'a>(
        program: Option<&str>,
        arguments: &[String],
        environment: impl IntoIterator<Item = (&'a str, &'a str)>,
        working_directory: Option<&Path>,
        worktrees: ScannedWorktrees,
    ) -> Self {
        let mut environment_candidates = Vec::new();
        for (_, value) in environment {
            push_environment_candidates(value, &mut environment_candidates);
        }
        let command = (program.is_some() || !arguments.is_empty())
            .then(|| parse_command_line(program, arguments));
        Self {
            command,
            environment_candidates,
            working_directory: working_directory.map(Path::to_path_buf),
            worktrees,
            program_resolves_like_language_servers: false,
        }
    }

    /// Looks the program up the way `lsp.<name>.binary.path` is resolved.
    pub(crate) fn resolving_program_like_language_servers(mut self) -> Self {
        self.program_resolves_like_language_servers = true;
        self
    }

    async fn run(&self, fs: Option<&Arc<dyn Fs>>) -> ScannedContent {
        let mut content = ScannedContent::default();
        if self.command.is_none() && self.environment_candidates.is_empty() {
            return content;
        }
        let lookup =
            WorktreeLookup::new(fs, &self.worktrees, self.working_directory.as_deref()).await;
        for root in &lookup.unchecked_roots {
            // Without the root's identity, a path spelled differently into
            // it can go unnoticed.
            content.session_only = true;
            content.push_reference(display_path(root), UnboundWorktreeContent::Unreadable);
        }

        if let Some(command) = &self.command {
            let program_bound = match self.program_candidate(command) {
                Some(program) => lookup.bind(&program, &mut content).await,
                None => false,
            };
            let script_bound = match &command.script {
                Some(script) => lookup.bind(script, &mut content).await,
                None => false,
            };
            if let Some(wrapper) = &command.wrapper {
                content
                    .unbound
                    .push(UnboundWorktreeContent::Wrapper(wrapper.clone().into()));
            }
            let covered = command.wrapper.is_none()
                && program_bound
                && (!command.runs_script || script_bound);
            if !covered
                && let (Some(program), Some(working_directory)) =
                    (&command.program, &self.working_directory)
                && lookup.contains_directory(working_directory).await
            {
                content
                    .unbound
                    .push(UnboundWorktreeContent::WorkingDirectory {
                        program: program.clone().into(),
                        working_directory: display_path(working_directory),
                    });
            }
            for file in &command.argument_files {
                if let Some(found) = lookup.find(file, Spelling::Explicit).await {
                    content
                        .push_reference(found.display_path, UnboundWorktreeContent::ArgumentFile);
                }
            }
        }

        let references = self
            .command
            .iter()
            .flat_map(|command| &command.references)
            .chain(&self.environment_candidates);
        for candidate in references {
            let Some(found) = lookup.find(candidate, Spelling::Loose).await else {
                continue;
            };
            let unbound: fn(SharedString) -> UnboundWorktreeContent = match found.kind {
                FoundKind::Directory => UnboundWorktreeContent::Directory,
                FoundKind::RemoteFile => UnboundWorktreeContent::RemoteFile,
                FoundKind::LocalFile(_) | FoundKind::UncheckedLocalFile => {
                    UnboundWorktreeContent::File
                }
            };
            content.push_reference(found.display_path, unbound);
        }
        content
    }

    /// A path names a file, while a bare name is looked up on `PATH` (a
    /// language server binary is also looked up in the worktree root).
    fn program_candidate(&self, command: &CommandLineParts) -> Option<String> {
        let program = command.program.as_deref()?;
        if self.program_resolves_like_language_servers {
            if let Some(rest) = program.strip_prefix("~/") {
                return Some(paths::home_dir().join(rest).to_string_lossy().into_owned());
            }
            return Some(program.to_string());
        }
        program.contains(['/', '\\']).then(|| program.to_string())
    }
}

/// What a command line runs, as far as its text tells.
#[derive(Clone, Default)]
struct CommandLineParts {
    /// The program that runs, after `env` and its assignments. `None` for a
    /// language server's own binary with arguments from settings.
    program: Option<String>,
    /// Whether the program is an interpreter or runner, whose script decides
    /// what runs.
    runs_script: bool,
    /// An interpreter's or runner's script: its first argument that is
    /// neither an option nor an option's value.
    script: Option<String>,
    /// A wrapper that picks what runs on its own terms.
    wrapper: Option<String>,
    /// Files that `@file` arguments read more arguments from.
    argument_files: Vec<String>,
    /// Everything else that could name a worktree path.
    references: Vec<String>,
}

fn parse_command_line<'a>(program: Option<&'a str>, arguments: &'a [String]) -> CommandLineParts {
    let mut parts = CommandLineParts::default();
    let Some(mut program) = program else {
        push_arguments(arguments, &mut parts);
        return parts;
    };
    let mut arguments = arguments;
    // `env` runs its first operand, after its options and assignments.
    while is_bare_or_absolute(program) && program_stem(program) == "env" {
        if program.contains(['/', '\\']) {
            parts.references.push(program.to_string());
        }
        let operand = match env_operand(arguments, &mut parts) {
            EnvOperand::Program(index) => arguments.get(index).zip(arguments.get(index + 1..)),
            EnvOperand::Wrapper => {
                parts.wrapper = Some(program.to_string());
                parts.program = Some(program.to_string());
                push_arguments(arguments, &mut parts);
                return parts;
            }
            EnvOperand::Nothing => None,
        };
        let Some((operand, rest)) = operand else {
            parts.program = Some(program.to_string());
            return parts;
        };
        program = operand;
        arguments = rest;
    }

    parts.program = Some(program.to_string());
    let stem = program_stem(program);
    let first_operand = arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map(String::as_str);
    let is_wrapper = stem == "wsl"
        || (stem == "direnv" && first_operand == Some("exec"))
        || (stem == "mise" && matches!(first_operand, Some("exec" | "x")));
    if is_wrapper && is_bare_or_absolute(program) {
        parts.wrapper = Some(program.to_string());
        push_arguments(arguments, &mut parts);
    } else if runs_code_from_working_directory(program) {
        parts.runs_script = true;
        let script = script_argument(&stem, arguments);
        for (index, argument) in arguments.iter().enumerate() {
            if Some(index) == script {
                parts.script = Some(argument.clone());
            } else {
                push_argument(argument, &mut parts);
            }
        }
    } else {
        push_arguments(arguments, &mut parts);
    }
    parts
}

enum EnvOperand {
    Program(usize),
    Wrapper,
    Nothing,
}

/// Where `env` finds the program it runs. The values it assigns are looked
/// up like any environment value.
fn env_operand(arguments: &[String], parts: &mut CommandLineParts) -> EnvOperand {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let argument = argument.as_str();
        if argument == "--" {
            return EnvOperand::Program(index + 1);
        } else if argument == "-u" || argument == "--unset" {
            index += 2;
        } else if argument.starts_with("--split-string")
            || argument.starts_with("--chdir")
            || (!argument.starts_with("--")
                && (argument.starts_with("-S") || argument.starts_with("-C")))
        {
            // A string split into a command line, or another working
            // directory, decides what runs.
            return EnvOperand::Wrapper;
        } else if argument.starts_with('-') {
            index += 1;
        } else if let Some((name, value)) = argument.split_once('=')
            && !name.is_empty()
        {
            push_environment_candidates(value, &mut parts.references);
            index += 1;
        } else {
            return EnvOperand::Program(index);
        }
    }
    EnvOperand::Nothing
}

fn script_convention(stem: &str) -> &'static ScriptConvention {
    match stem {
        "node" => &NODE,
        "uv" => &UV,
        "uvx" => &UVX,
        "deno" => &DENO,
        "bun" => &BUN,
        "npm" | "npx" | "pnpm" | "yarn" => &NPM,
        "pwsh" | "powershell" => &POWERSHELL,
        "cmd" => &CMD,
        "bash" | "sh" | "zsh" => &SHELL,
        "ruby" => &RUBY,
        "perl" => &PERL,
        "php" => &PHP,
        _ if stem == "py" || stem.starts_with("python") => &PYTHON,
        _ => &NO_CONVENTION,
    }
}

impl ScriptConvention {
    fn matches(&self, options: &[&str], argument: &str) -> bool {
        options.iter().any(|option| {
            if self.ignores_case {
                option.eq_ignore_ascii_case(argument)
            } else {
                *option == argument
            }
        })
    }

    /// Whether `script` can be what runs, rather than a file an option
    /// missing from the tables took as its value.
    fn looks_like_entrypoint(&self, script: &str, after_subcommand: bool) -> bool {
        let extension = Path::new(script)
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
        let extension = extension.as_deref();
        if extension.is_some_and(|extension| NOT_ENTRYPOINT_EXTENSIONS.contains(&extension)) {
            return false;
        }
        if (after_subcommand || self.always_checks_extension)
            && !self.entrypoint_extensions.is_empty()
        {
            return extension
                .is_some_and(|extension| self.entrypoint_extensions.contains(&extension));
        }
        true
    }
}

/// The index of the script an interpreter or runner runs: its first argument
/// that is neither an option nor an option's value, and that looks like
/// something it runs. `None` when it runs code given inline (`-c`), a module
/// (`-m`), or reads its arguments from a file, and when what stands there
/// does not look like a script for it: nothing is bound then, so the
/// launch keeps its warnings.
fn script_argument(stem: &str, arguments: &[String]) -> Option<usize> {
    let convention = script_convention(stem);
    let accept = |index: usize, after_subcommand: bool| {
        arguments
            .get(index)
            .filter(|script| convention.looks_like_entrypoint(script, after_subcommand))
            .map(|_| index)
    };
    let mut subcommand_seen = false;
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let argument = argument.as_str();
        if argument == "--" || convention.matches(convention.script_options, argument) {
            return accept(index + 1, subcommand_seen);
        } else if argument.starts_with('@')
            || convention.matches(convention.inline_code_options, argument)
            || convention.matches(convention.module_options, argument)
        {
            return None;
        } else if convention.matches(convention.options_with_a_value, argument) {
            index += 2;
        } else if argument.starts_with('-')
            || (convention.slash_options && argument.starts_with('/'))
        {
            index += 1;
        } else if !subcommand_seen && convention.matches(convention.inline_subcommands, argument) {
            return None;
        } else if !subcommand_seen && convention.matches(convention.subcommands, argument) {
            subcommand_seen = true;
            index += 1;
        } else {
            return accept(index, subcommand_seen);
        }
    }
    None
}

fn push_arguments(arguments: &[String], parts: &mut CommandLineParts) {
    for argument in arguments {
        push_argument(argument, parts);
    }
}

/// An `@file` argument names a file of more arguments; anything else is
/// looked up whole and, for `--flag=value`, by its value.
fn push_argument(argument: &str, parts: &mut CommandLineParts) {
    let argument = argument.trim_matches(|character| character == '"' || character == '\'');
    match argument.strip_prefix('@') {
        Some(file) if !file.is_empty() => parts.argument_files.push(file.to_string()),
        _ => push_path_candidates(argument, &mut parts.references),
    }
}

fn push_environment_candidates(value: &str, candidates: &mut Vec<String>) {
    for token in value.split(|character: char| character.is_whitespace() || character == ';') {
        push_path_candidates(token, candidates);
        if token.contains(':') {
            for piece in token.split(':') {
                push_path_candidates(piece, candidates);
            }
        }
    }
}

/// A part of a command line is looked up whole and, for `--flag=value`, by
/// its value.
fn push_path_candidates(part: &str, candidates: &mut Vec<String>) {
    let part = part.trim_matches(|character| character == '"' || character == '\'');
    if part.is_empty() {
        return;
    }
    candidates.push(part.to_string());
    if let Some((_, value)) = part.split_once('=')
        && !value.is_empty()
    {
        candidates.push(value.to_string());
    }
}

#[derive(Default)]
struct ScannedContent {
    /// The program and script files the key is bound to, with their
    /// fingerprints.
    bound_files: Vec<(SharedString, String)>,
    /// The program or script, when it could not be bound by its contents,
    /// as part of the key.
    unbound_program_key_parts: Vec<String>,
    unbound: Vec<UnboundWorktreeContent>,
    session_only: bool,
    seen: HashSet<SharedString>,
}

impl ScannedContent {
    fn push_reference(
        &mut self,
        path: SharedString,
        unbound: fn(SharedString) -> UnboundWorktreeContent,
    ) {
        if self.seen.insert(path.clone()) {
            self.unbound.push(unbound(path));
        }
    }
}

struct Found {
    display_path: SharedString,
    kind: FoundKind,
}

enum FoundKind {
    /// A local worktree file, found by its identity.
    LocalFile(PathBuf),
    /// A local path found only by its spelling, or one whose metadata could
    /// not be read.
    UncheckedLocalFile,
    Directory,
    RemoteFile,
}

/// How a remote path has to be spelled to count without the worktree's
/// snapshot having it, as a directory that is gitignored or not loaded yet
/// or a symlink does not.
#[derive(Clone, Copy)]
enum Spelling {
    /// A path, rather than a bare name, an option or a URL.
    Loose,
    /// An absolute path or one starting with `./` or `../`, as for `@file`,
    /// which also starts npm package names.
    Explicit,
}

impl Spelling {
    fn counts_without_entry(self, candidate: &str, path_style: PathStyle) -> bool {
        let explicit = path_style.is_absolute(candidate)
            || ["./", "../", ".\\", "..\\"]
                .iter()
                .any(|prefix| candidate.starts_with(prefix));
        match self {
            Self::Explicit => explicit,
            Self::Loose => {
                !candidate.starts_with('-')
                    && !candidate.contains("://")
                    && (explicit || candidate.starts_with('.') || candidate.contains(['/', '\\']))
            }
        }
    }
}

/// Finds what a command line names in the worktrees.
struct WorktreeLookup<'a> {
    fs: Option<&'a Arc<dyn Fs>>,
    worktrees: &'a ScannedWorktrees,
    working_directory: Option<&'a Path>,
    /// Identities of the local roots that could be read.
    root_identities: Vec<u64>,
    /// Local roots whose identity could not be read (all of them without a
    /// file system), which paths are compared with by spelling instead.
    unchecked_roots: Vec<PathBuf>,
}

impl<'a> WorktreeLookup<'a> {
    async fn new(
        fs: Option<&'a Arc<dyn Fs>>,
        worktrees: &'a ScannedWorktrees,
        working_directory: Option<&'a Path>,
    ) -> Self {
        let mut root_identities = Vec::new();
        let mut unchecked_roots = Vec::new();
        for root in &worktrees.local_roots {
            let identity = match fs {
                Some(fs) => match fs.canonicalize(root).await {
                    Ok(canonical) => fs
                        .metadata(&canonical)
                        .await
                        .ok()
                        .flatten()
                        .map(|metadata| metadata.inode),
                    Err(_) => None,
                },
                None => None,
            };
            match identity {
                Some(identity) => root_identities.push(identity),
                None => {
                    log::warn!("could not read the identity of worktree {root:?}");
                    unchecked_roots.push(root.clone());
                }
            }
        }
        Self {
            fs,
            worktrees,
            working_directory,
            root_identities,
            unchecked_roots,
        }
    }

    async fn find(&self, candidate: &str, spelling: Spelling) -> Option<Found> {
        if let Some(found) = self.find_local(candidate).await {
            return Some(found);
        }
        self.worktrees
            .remote
            .iter()
            .find_map(|snapshot| find_remote(snapshot, candidate, self.working_directory, spelling))
    }

    async fn find_local(&self, candidate: &str) -> Option<Found> {
        if self.worktrees.local_roots.is_empty() {
            return None;
        }
        let candidate = Path::new(candidate);
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.working_directory?.join(candidate)
        };
        let unchecked = |path: &Path| Found {
            display_path: display_path(path),
            kind: FoundKind::UncheckedLocalFile,
        };
        let Some(fs) = self.fs else {
            return self
                .inside_unchecked_root(&absolute)
                .then(|| unchecked(&absolute));
        };
        let canonical = match fs.canonicalize(&absolute).await {
            Ok(canonical) => canonical,
            // Missing, or there but unreadable: only the latter can run.
            Err(_) => {
                return (!matches!(fs.metadata(&absolute).await, Ok(None))
                    && inside_any_root_by_spelling(&absolute, &self.worktrees.local_roots))
                .then(|| unchecked(&absolute));
            }
        };
        match fs.metadata(&canonical).await {
            Ok(None) => None,
            Ok(Some(metadata)) => {
                let by_identity =
                    is_inside_local_root(fs.as_ref(), &canonical, &self.root_identities).await;
                let by_spelling = !by_identity
                    && (self.inside_unchecked_root(&canonical)
                        || self.inside_unchecked_root(&absolute));
                let kind = match (metadata.is_dir, by_identity, by_spelling) {
                    (_, false, false) => return None,
                    (true, _, _) => FoundKind::Directory,
                    (false, true, _) => FoundKind::LocalFile(canonical.clone()),
                    (false, false, true) => FoundKind::UncheckedLocalFile,
                };
                Some(Found {
                    display_path: display_path(&canonical),
                    kind,
                })
            }
            // There but unreadable: it counts wherever its spelling puts it.
            Err(_) => (inside_any_root_by_spelling(&canonical, &self.worktrees.local_roots)
                || inside_any_root_by_spelling(&absolute, &self.worktrees.local_roots))
            .then(|| unchecked(&canonical)),
        }
    }

    fn inside_unchecked_root(&self, path: &Path) -> bool {
        inside_any_root_by_spelling(path, &self.unchecked_roots)
    }

    /// Whether `directory` is in a worktree, however it is spelled.
    async fn contains_directory(&self, directory: &Path) -> bool {
        self.worktrees.contains(directory)
            || self
                .find_local(&directory.to_string_lossy())
                .await
                .is_some()
    }

    /// Binds the program or script `candidate` by its contents, and returns
    /// whether the key now covers it. One that cannot be bound is recorded
    /// as part of the key instead, and makes the approval session-only
    /// unless it is a directory.
    async fn bind(&self, candidate: &str, content: &mut ScannedContent) -> bool {
        let Some(found) = self.find(candidate, Spelling::Loose).await else {
            return false;
        };
        let path = found.display_path;
        if !content.seen.insert(path.clone()) {
            // The program named again as its own script.
            return content.bound_files.iter().any(|(bound, _)| bound == &path);
        }
        let (label, unbound, session_only) = match found.kind {
            FoundKind::LocalFile(file) => match self.fingerprint(&file).await {
                Ok(fingerprint) => {
                    content.bound_files.push((path, fingerprint));
                    return true;
                }
                Err(error) => {
                    log::warn!("could not read {file:?} for a launch approval key: {error:#}");
                    (
                        "unreadable",
                        UnboundWorktreeContent::Unreadable(path.clone()),
                        true,
                    )
                }
            },
            FoundKind::UncheckedLocalFile => (
                "unreadable",
                UnboundWorktreeContent::Unreadable(path.clone()),
                true,
            ),
            FoundKind::RemoteFile => (
                "remote-file",
                UnboundWorktreeContent::RemoteFile(path.clone()),
                true,
            ),
            FoundKind::Directory => (
                "directory",
                UnboundWorktreeContent::Directory(path.clone()),
                false,
            ),
        };
        content
            .unbound_program_key_parts
            .push(format!("{label}:{path}"));
        content.session_only |= session_only;
        content.unbound.push(unbound);
        false
    }

    async fn fingerprint(&self, path: &Path) -> Result<String> {
        let fs = self.fs.context("no file system to read the file from")?;
        if fs.is_fake() {
            // The fake file system of tests holds no special files.
            let metadata = fs.metadata(path).await?.context("the file is gone")?;
            anyhow::ensure!(!metadata.is_dir && !metadata.is_fifo, "not a regular file");
            anyhow::ensure!(
                metadata.len <= MAX_BOUND_FILE_SIZE,
                "larger than {MAX_BOUND_FILE_SIZE} bytes"
            );
            let contents = fs.load_bytes(path).await?;
            return Ok(format!("sha256:{:x}", Sha256::digest(&contents)));
        }
        let path = path.to_path_buf();
        smol::unblock(move || hash_regular_file(&path, MAX_BOUND_FILE_SIZE)).await
    }
}

/// Hashes a regular file without ever waiting on something that is not one:
/// only a regular file is opened (a FIFO swapped in between is opened
/// without blocking and refused), what gets opened must be the file that
/// was checked, and no more than `size_limit` bytes are read.
fn hash_regular_file(path: &Path, size_limit: u64) -> Result<String> {
    use std::io::Read as _;

    let before_opening = std::fs::metadata(path)?;
    anyhow::ensure!(before_opening.file_type().is_file(), "not a regular file");
    anyhow::ensure!(
        before_opening.len() <= size_limit,
        "larger than {size_limit} bytes"
    );
    #[cfg(unix)]
    let identity_before_opening = unix_file_identity(&before_opening);
    #[cfg(windows)]
    let identity_before_opening = windows_identity_before_opening(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    anyhow::ensure!(opened.file_type().is_file(), "not a regular file");
    #[cfg(unix)]
    ensure_same_file(identity_before_opening, unix_file_identity(&opened))?;
    #[cfg(windows)]
    ensure_same_file(identity_before_opening, windows_file_identity(&file)?)?;
    let mut reader = file.take(size_limit.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        total = total.saturating_add(read as u64);
        anyhow::ensure!(total <= size_limit, "grew beyond {size_limit} bytes");
        hasher.update(buffer.get(..read).context("read past the buffer")?);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// What tells two files apart: the device and inode on Unix, the volume
/// serial number and file index on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    index: u64,
}

fn ensure_same_file(before_opening: FileIdentity, opened: FileIdentity) -> Result<()> {
    anyhow::ensure!(
        before_opening == opened,
        "the file was replaced while being opened"
    );
    Ok(())
}

#[cfg(unix)]
fn unix_file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        device: metadata.dev(),
        index: metadata.ino(),
    }
}

/// The identity of the file at `path` itself, read through a handle for its
/// attributes only that does not follow a reparse point there: a link that
/// appeared after the path was canonicalized is refused, not followed.
#[cfg(windows)]
fn windows_identity_before_opening(path: &Path) -> Result<FileIdentity> {
    use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FileAttributeTagInfo, GetFileInformationByHandleEx,
        },
    };
    // Symbolic links, junctions and mount points name another file; other
    // reparse points (cloud placeholders, deduplicated files) are the file.
    const NAME_SURROGATE: u32 = 0x2000_0000;

    let probe = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?;
    // SAFETY: the handle stays open for the call, and the structure is plain
    // data of the size given, which the call fills in.
    let mut tag_information: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(probe.as_raw_handle() as _),
            FileAttributeTagInfo,
            (&mut tag_information as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )?
    };
    anyhow::ensure!(
        tag_information.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            || tag_information.ReparseTag & NAME_SURROGATE == 0,
        "a link, which is not followed"
    );
    windows_file_identity(&probe)
}

#[cfg(windows)]
fn windows_file_identity(file: &std::fs::File) -> Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
    };

    // SAFETY: the handle stays open for the call, and the structure is plain
    // data, which the call fills in.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut information)? };
    Ok(FileIdentity {
        device: u64::from(information.dwVolumeSerialNumber),
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

/// Compares file identities rather than paths, so that no spelling of the
/// same place — letter case, `\\?\` and UNC forms, 8.3 names, links — gets
/// out of a worktree.
async fn is_inside_local_root(fs: &dyn Fs, path: &Path, root_identities: &[u64]) -> bool {
    if root_identities.is_empty() {
        return false;
    }
    for ancestor in path.ancestors() {
        if let Ok(Some(metadata)) = fs.metadata(ancestor).await
            && root_identities.contains(&metadata.inode)
        {
            return true;
        }
    }
    false
}

fn inside_any_root_by_spelling(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| is_inside_by_spelling(path, root))
}

/// Whether `path` lies in `root` as spelled, with `.` and `..` resolved and,
/// on Windows, letter case ignored.
fn is_inside_by_spelling(path: &Path, root: &Path) -> bool {
    match (spelled_components(path), spelled_components(root)) {
        (Some(path), Some(root)) => path.starts_with(&root),
        _ => false,
    }
}

fn spelled_components(path: &Path) -> Option<Vec<String>> {
    let path = display_path(path);
    let normalized = util::paths::normalize_lexically(Path::new(path.as_ref())).ok()?;
    Some(
        normalized
            .components()
            .map(|component| {
                let component = component.as_os_str().to_string_lossy();
                if cfg!(windows) {
                    component.to_lowercase()
                } else {
                    component.into_owned()
                }
            })
            .collect(),
    )
}

/// Any path spelled into a remote worktree counts, `..` resolved: the
/// snapshot here lacks what is gitignored or not loaded yet, and cannot tell
/// where a symlink leads.
fn find_remote(
    snapshot: &Snapshot,
    candidate: &str,
    working_directory: Option<&Path>,
    spelling: Spelling,
) -> Option<Found> {
    let path_style = snapshot.path_style();
    let absolute = if path_style.is_absolute(candidate) {
        candidate.to_string()
    } else {
        path_style.join(working_directory?, candidate)?
    };
    let absolute = normalize_lexically(&absolute, path_style);
    let relative = path_style.strip_prefix(Path::new(&absolute), snapshot.abs_path().as_ref())?;
    let kind = match snapshot.entry_for_path(&relative) {
        Some(entry) if entry.is_dir() => FoundKind::Directory,
        Some(_) => FoundKind::RemoteFile,
        None if spelling.counts_without_entry(candidate, path_style) => FoundKind::RemoteFile,
        None => return None,
    };
    Some(Found {
        display_path: absolute.into(),
        kind,
    })
}

/// Resolves `.` and `..` in a path on a remote host, which cannot be
/// canonicalized from here.
fn normalize_lexically(path: &str, path_style: PathStyle) -> String {
    if !path_style.is_posix() {
        return path.to_string();
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }
    format!("/{}", components.join("/"))
}

fn display_path(path: &Path) -> SharedString {
    let text = path.to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    };
    text.into()
}

/// A definition that came from a settings file a sandboxed command could
/// have written.
pub(crate) struct UntrustedDefinition {
    pub(crate) file: LaunchDefinitionFile,
    pub(crate) replaces_user_entry: bool,
}

pub(crate) fn worktree_settings_file(
    worktree: &Worktree,
    directory: &RelPath,
) -> LaunchDefinitionFile {
    let path_style = worktree.path_style();
    let relative = directory.join(paths::local_settings_file_relative_path());
    let abs_path = worktree.abs_path();
    let path = path_style
        .join(&abs_path, &*relative.display(path_style))
        .unwrap_or_else(|| {
            abs_path
                .join(relative.as_std_path())
                .to_string_lossy()
                .into_owned()
        });
    LaunchDefinitionFile::Worktree(path.into())
}

/// Where `lsp.<server_name>.binary` comes from, when the launch-relevant
/// part of it (path, arguments or environment) is set by an untrusted file.
pub(crate) fn untrusted_language_server_binary(
    settings_store: &SettingsStore,
    worktree: &Worktree,
    server_name: &str,
) -> Option<UntrustedDefinition> {
    let sets_binary = |content: &ProjectSettingsContent| {
        content
            .lsp
            .0
            .get(server_name)
            .and_then(|settings| settings.binary.as_ref())
            .is_some_and(|binary| {
                binary.path.is_some() || binary.arguments.is_some() || binary.env.is_some()
            })
    };
    let defined_in = |file: SettingsFile| {
        settings_store
            .get_content_for_file(file)
            .is_some_and(|content| sets_binary(&content.project))
    };
    let file = if defined_in(SettingsFile::Project((worktree.id(), RelPath::empty_arc()))) {
        worktree_settings_file(worktree, RelPath::empty())
    } else if defined_in(SettingsFile::Server) {
        LaunchDefinitionFile::RemoteServerSettings
    } else {
        return None;
    };
    Some(UntrustedDefinition {
        file,
        replaces_user_entry: defined_in(SettingsFile::User) || defined_in(SettingsFile::Global),
    })
}

/// Where an external formatter in effect for the buffer at `buffer_path`
/// comes from, when an untrusted file names it.
pub(crate) fn untrusted_external_formatter(
    settings_store: &SettingsStore,
    worktree: &Worktree,
    buffer_path: &RelPath,
    language_name: Option<&str>,
    formatter: &Formatter,
) -> Option<UntrustedDefinition> {
    let names_formatter = |content: &ProjectSettingsContent| {
        let language_formatter = language_name
            .and_then(|name| content.all_languages.languages.0.get(name))
            .and_then(|language| language.formatter.as_ref());
        [
            content.all_languages.defaults.formatter.as_ref(),
            language_formatter,
        ]
        .into_iter()
        .flatten()
        .any(|list: &FormatterList| list.as_ref().contains(formatter))
    };
    let file = settings_store
        .local_settings(worktree.id())
        .find(|(directory, content)| buffer_path.starts_with(directory) && names_formatter(content))
        .map(|(directory, _)| worktree_settings_file(worktree, &directory))
        .or_else(|| {
            settings_store
                .get_content_for_file(SettingsFile::Server)
                .filter(|content| names_formatter(&content.project))
                .map(|_| LaunchDefinitionFile::RemoteServerSettings)
        })?;
    let replaces_user_entry = [SettingsFile::User, SettingsFile::Global]
        .into_iter()
        .any(|file| {
            settings_store
                .get_content_for_file(file)
                .is_some_and(|content| names_formatter(&content.project))
        });
    Some(UntrustedDefinition {
        file,
        replaces_user_entry,
    })
}

#[derive(Clone, Debug)]
pub enum ProjectLaunchApprovalsEvent {
    /// A launch that needs approval was asked for, and no notification about
    /// the same launch is live. Emitted at most once per key, and never for a
    /// denied key.
    PendingLaunchAppeared(LaunchApprovalDetails),
    /// The definition of a pending launch changed while it waited: `new`
    /// replaces the launch keyed `old`, which can no longer be decided. It
    /// stands in for a new notification about the same launch.
    PendingLaunchReplaced {
        old: LaunchApprovalKey,
        new: LaunchApprovalDetails,
    },
    /// Nothing asks for this pending launch any more.
    PendingLaunchWithdrawn(LaunchApprovalKey),
    LaunchDecided {
        key: LaunchApprovalKey,
        decision: LaunchDecision,
    },
    LaunchApprovalRevoked(LaunchApprovalKey),
}

/// The launch a pending entry stands for, across edits of its definition.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LaunchSlot {
    worktree_abs_path: Arc<Path>,
    host: Option<SharedString>,
    kind: LaunchKind,
    name: SharedString,
}

impl LaunchSlot {
    pub(crate) fn of(details: &LaunchApprovalDetails) -> Self {
        Self {
            worktree_abs_path: details.worktree_abs_path.clone(),
            host: details
                .host
                .identity()
                .map(|identity| SharedString::from(identity.to_string())),
            kind: details.kind,
            name: details.name.clone(),
        }
    }
}

enum StoredDecisions {
    Loading,
    Loaded,
    Unavailable(SharedString),
}

/// Pending launches are withdrawn with this when whatever waited for them
/// goes away, for example a language server that is stopped first.
pub struct PendingLaunchGuard {
    key: LaunchApprovalKey,
    requester: EntityId,
    withdrawals: mpsc::UnboundedSender<(LaunchApprovalKey, EntityId)>,
}

impl PendingLaunchGuard {
    pub(crate) fn new(
        key: LaunchApprovalKey,
        requester: EntityId,
        withdrawals: mpsc::UnboundedSender<(LaunchApprovalKey, EntityId)>,
    ) -> Self {
        Self {
            key,
            requester,
            withdrawals,
        }
    }

    /// Makes the guard stand for `key` from now on, returning the key it
    /// stood for, which the caller withdraws itself if it is still pending.
    pub fn replace_key(&mut self, key: LaunchApprovalKey) -> LaunchApprovalKey {
        std::mem::replace(&mut self.key, key)
    }
}

impl Drop for PendingLaunchGuard {
    fn drop(&mut self) {
        if self
            .withdrawals
            .unbounded_send((self.key.clone(), self.requester))
            .is_err()
        {
            log::debug!("launch approvals are gone; nothing to withdraw");
        }
    }
}

/// Marks the remote server, which has no approvals of its own: it asks the
/// IDE of the session a project belongs to (ADR-F50), and never decides by
/// itself.
struct HeadlessProcess;

impl Global for HeadlessProcess {}

/// Who decides about project-defined language servers and formatters.
#[derive(Clone)]
pub(crate) enum LaunchApprover {
    /// This IDE's app-wide approvals.
    Local(Entity<ProjectLaunchApprovals>),
    /// The IDE of the remote server's session (ADR-F50).
    Remote(Entity<RemoteLaunchApprovals>),
}

/// What asking about a launch came to.
pub(crate) enum LaunchAnswer {
    Approved(LaunchApprovalKey),
    Denied,
    /// Resolves with the decision, or is canceled when the launch stops
    /// being pending some other way, after which the caller looks again.
    Pending(oneshot::Receiver<LaunchDecision>),
    /// Nobody could be asked: nothing is launched.
    Unavailable(SharedString),
    /// The IDE did not answer; `answered` resolves once it answers anything,
    /// after which the caller asks again.
    Unanswered {
        reason: SharedString,
        answered: oneshot::Receiver<()>,
    },
    /// The IDE has no room now for another launch to wait for the user;
    /// `room` resolves once it may have, after which the caller asks again.
    Deferred {
        reason: SharedString,
        room: oneshot::Receiver<()>,
    },
}

impl LaunchApprover {
    /// Asks about `details` on behalf of `requester`. While the launch is
    /// pending, `pending_guard` stands for it: dropping the guard withdraws
    /// it.
    pub(crate) async fn ask(
        &self,
        details: LaunchApprovalDetails,
        place: &LaunchPlace,
        requester: EntityId,
        pending_guard: &mut Option<PendingLaunchGuard>,
        cx: &mut AsyncApp,
    ) -> LaunchAnswer {
        match self {
            Self::Local(launch_approvals) => launch_approvals.update(cx, |launch_approvals, cx| {
                let details = launch_approvals.request_launch(details, requester, cx);
                match details.state {
                    LaunchApprovalState::Approved => LaunchAnswer::Approved(details.key),
                    LaunchApprovalState::Denied => LaunchAnswer::Denied,
                    LaunchApprovalState::Pending => {
                        match pending_guard {
                            Some(guard) => {
                                let previous = guard.replace_key(details.key.clone());
                                if previous != details.key {
                                    launch_approvals.withdraw_pending(&previous, requester, cx);
                                }
                            }
                            None => {
                                *pending_guard = Some(
                                    launch_approvals.pending_launch_guard(&details.key, requester),
                                );
                            }
                        }
                        LaunchAnswer::Pending(launch_approvals.wait_for_decision(&details.key))
                    }
                }
            }),
            Self::Remote(launch_approvals) => loop {
                let (answer, withdrawals_before) =
                    launch_approvals.update(cx, |launch_approvals, cx| {
                        (
                            launch_approvals.request_launch(
                                &details,
                                place,
                                LANGUAGE_SERVER_ANSWER_TIMEOUT,
                                cx,
                            ),
                            launch_approvals.withdrawals_sent(&details.key),
                        )
                    });
                let answer = answer.await;
                let key = details.key.clone();
                return match answer {
                    RemoteAnswer::Approved => LaunchAnswer::Approved(key),
                    RemoteAnswer::Denied => LaunchAnswer::Denied,
                    RemoteAnswer::Unavailable(reason) => LaunchAnswer::Unavailable(reason),
                    RemoteAnswer::Unanswered {
                        reason,
                        answers_seen,
                    } => LaunchAnswer::Unanswered {
                        reason,
                        answered: launch_approvals.update(cx, |launch_approvals, _| {
                            launch_approvals.wait_until_the_ide_answers(answers_seen)
                        }),
                    },
                    RemoteAnswer::Deferred(reason) => LaunchAnswer::Deferred {
                        reason,
                        room: launch_approvals.update(cx, |launch_approvals, cx| {
                            launch_approvals.wait_for_room(cx)
                        }),
                    },
                    RemoteAnswer::Pending => {
                        let decision = launch_approvals.update(cx, |launch_approvals, cx| {
                            // A withdrawal sent for an earlier start while the
                            // question was out can reach the IDE after it, and
                            // take back what the IDE answered.
                            let withdrawn_since_asked =
                                launch_approvals.withdrawals_sent(&key) > withdrawals_before;
                            match pending_guard {
                                Some(guard) => {
                                    let previous = guard.replace_key(key.clone());
                                    if previous != key {
                                        launch_approvals.withdraw_pending(&previous, requester, cx);
                                    }
                                    launch_approvals.keep_pending(&key, requester);
                                }
                                None => {
                                    *pending_guard = Some(
                                        launch_approvals.pending_launch_guard(&key, requester),
                                    );
                                }
                            }
                            (!withdrawn_since_asked)
                                .then(|| launch_approvals.wait_for_decision(&key))
                        });
                        let Some(decision) = decision else {
                            continue;
                        };
                        LaunchAnswer::Pending(decision)
                    }
                };
            },
        }
    }

    /// Asks once, for a launch that does not wait (a formatter is skipped
    /// instead): whether it may run now, or why nobody could be asked.
    pub(crate) async fn ask_once(
        &self,
        details: LaunchApprovalDetails,
        place: &LaunchPlace,
        requester: EntityId,
        cx: &mut AsyncApp,
    ) -> Result<bool, SharedString> {
        match self {
            Self::Local(launch_approvals) => Ok(launch_approvals
                .update(cx, |launch_approvals, cx| {
                    launch_approvals.request_launch(details, requester, cx)
                })
                .state
                == LaunchApprovalState::Approved),
            Self::Remote(launch_approvals) => {
                let answer = launch_approvals
                    .update(cx, |launch_approvals, cx| {
                        launch_approvals.request_launch(
                            &details,
                            place,
                            FORMATTER_ANSWER_TIMEOUT,
                            cx,
                        )
                    })
                    .await;
                match answer {
                    RemoteAnswer::Approved => Ok(true),
                    RemoteAnswer::Denied | RemoteAnswer::Pending => Ok(false),
                    RemoteAnswer::Unavailable(reason)
                    | RemoteAnswer::Deferred(reason)
                    | RemoteAnswer::Unanswered { reason, .. } => Err(reason),
                }
            }
        }
    }

    /// Whether `key` is still approved: a decision taken back while its
    /// approval was on its way stops the launch it approved.
    pub(crate) fn still_approved(&self, key: &LaunchApprovalKey, cx: &App) -> bool {
        let decision = match self {
            Self::Local(launch_approvals) => launch_approvals.read(cx).decision(key),
            Self::Remote(launch_approvals) => launch_approvals.read(cx).decision(key),
        };
        decision == Some(LaunchDecision::Approved)
    }
}

/// Whether a project-defined language server or formatter may be asked about.
pub(crate) enum LaunchGate {
    Ask(LaunchApprover),
    /// Nobody to ask: nothing is launched.
    Unavailable,
}

/// `remote_launch_approvals` belongs to a language server store of the
/// remote server that is shared with an IDE.
pub(crate) fn launch_gate(
    remote_launch_approvals: Option<&Entity<RemoteLaunchApprovals>>,
    cx: &App,
) -> LaunchGate {
    if let Some(remote_launch_approvals) = remote_launch_approvals {
        LaunchGate::Ask(LaunchApprover::Remote(remote_launch_approvals.clone()))
    } else if cx.has_global::<HeadlessProcess>() {
        LaunchGate::Unavailable
    } else if let Some(launch_approvals) = ProjectLaunchApprovals::try_global(cx) {
        LaunchGate::Ask(LaunchApprover::Local(launch_approvals))
    } else {
        LaunchGate::Unavailable
    }
}

pub fn init(cx: &mut App) {
    ProjectLaunchApprovals::global(cx);
}

/// The app-wide record of which project-defined launches the user approved
/// or denied, backed by the database's key-value store. The data directory is
/// one of the paths the agent sandbox protects, unlike any settings file.
pub struct ProjectLaunchApprovals {
    decisions: HashMap<LaunchApprovalKey, LaunchDecisionRecord>,
    stored_decisions: StoredDecisions,
    pending: Vec<LaunchApprovalDetails>,
    pending_requesters: HashMap<LaunchApprovalKey, HashSet<EntityId>>,
    notified_keys: HashSet<LaunchApprovalKey>,
    notified_slots: HashSet<LaunchSlot>,
    decision_waiters: HashMap<LaunchApprovalKey, Vec<oneshot::Sender<LaunchDecision>>>,
    withdrawals: mpsc::UnboundedSender<(LaunchApprovalKey, EntityId)>,
    _load_decisions: Task<()>,
    _process_withdrawals: Task<()>,
    #[cfg(any(test, feature = "test-support"))]
    approve_everything: bool,
}

struct GlobalProjectLaunchApprovals(Entity<ProjectLaunchApprovals>);

impl Global for GlobalProjectLaunchApprovals {}

impl EventEmitter<ProjectLaunchApprovalsEvent> for ProjectLaunchApprovals {}

impl ProjectLaunchApprovals {
    fn new(cx: &mut Context<Self>) -> Self {
        let (withdrawals, mut withdrawal_requests) =
            mpsc::unbounded::<(LaunchApprovalKey, EntityId)>();
        let process_withdrawals = cx.spawn(async move |this, cx| {
            while let Some((key, requester)) = withdrawal_requests.next().await {
                if this
                    .update(cx, |this, cx| this.withdraw_pending(&key, requester, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let (stored_decisions, load_decisions) = match key_value_store(cx) {
            Ok(store) => {
                let rows =
                    cx.background_spawn(async move { store.scoped(DB_NAMESPACE).read_all() });
                let load_decisions = cx.spawn(async move |this, cx| {
                    let rows = rows.await;
                    this.update(cx, |this, cx| this.finish_loading(rows, cx))
                        .log_err();
                });
                (StoredDecisions::Loading, load_decisions)
            }
            Err(error) => (
                StoredDecisions::Unavailable(format!("{error:#}").into()),
                Task::ready(()),
            ),
        };
        Self {
            decisions: HashMap::default(),
            stored_decisions,
            pending: Vec::new(),
            pending_requesters: HashMap::default(),
            notified_keys: HashSet::default(),
            notified_slots: HashSet::default(),
            decision_waiters: HashMap::default(),
            withdrawals,
            _load_decisions: load_decisions,
            _process_withdrawals: process_withdrawals,
            #[cfg(any(test, feature = "test-support"))]
            approve_everything: false,
        }
    }

    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(approvals) = Self::try_global(cx) {
            return approvals;
        }
        let approvals = cx.new(Self::new);
        cx.set_global(GlobalProjectLaunchApprovals(approvals.clone()));
        approvals
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalProjectLaunchApprovals>()
            .map(|global| global.0.clone())
    }

    /// For the remote server, which asks the IDE of each session instead of
    /// deciding by itself (ADR-F50): without such an IDE, project-defined
    /// language servers and formatters do not run there.
    pub fn mark_process_headless(cx: &mut App) {
        cx.set_global(HeadlessProcess);
    }

    /// What the user decided about `key`, if anything.
    pub(crate) fn decision(&self, key: &LaunchApprovalKey) -> Option<LaunchDecision> {
        match self.state_of(key).0 {
            LaunchApprovalState::Approved => Some(LaunchDecision::Approved),
            LaunchApprovalState::Denied => Some(LaunchDecision::Denied),
            LaunchApprovalState::Pending => None,
        }
    }

    /// Lets `key` raise a notification again, for a launch nothing waits for
    /// any more: kept, the record of notified keys would only grow.
    pub(crate) fn forget_notification(&mut self, key: &LaunchApprovalKey) {
        self.notified_keys.remove(key);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn notified_key_count(&self) -> usize {
        self.notified_keys.len()
    }

    /// Launches waiting for a decision, oldest first. A launch whose
    /// definition changed while it waited shows up once, with the newest
    /// definition.
    pub fn pending_launches(&self) -> &[LaunchApprovalDetails] {
        &self.pending
    }

    pub fn pending_launch(&self, key: &LaunchApprovalKey) -> Option<&LaunchApprovalDetails> {
        self.pending.iter().find(|pending| &pending.key == key)
    }

    /// Looks up the decision for `details.key` and, unless there is one,
    /// records the launch as pending on behalf of `requester`. Returns
    /// `details` with `state` (and `store_error`) filled in; only `Approved`
    /// may launch.
    pub fn request_launch(
        &mut self,
        mut details: LaunchApprovalDetails,
        requester: EntityId,
        cx: &mut Context<Self>,
    ) -> LaunchApprovalDetails {
        let (state, store_error) = self.state_of(&details.key);
        details.state = state;
        details.store_error = store_error;
        if state != LaunchApprovalState::Pending {
            return details;
        }

        // A newer definition of the same launch replaces the older one, so an
        // edited definition (or one an agent varies on every save) leaves one
        // entry and one live notification behind, updated in place.
        let slot = LaunchSlot::of(&details);
        let superseded = self
            .pending
            .iter()
            .filter(|pending| pending.key != details.key && LaunchSlot::of(pending) == slot)
            .map(|pending| pending.key.clone())
            .collect::<Vec<_>>();
        let slot_was_notified = self.notified_slots.contains(&slot);
        for key in &superseded {
            self.remove_pending(key);
        }
        match self
            .pending
            .iter_mut()
            .find(|pending| pending.key == details.key)
        {
            Some(pending) => *pending = details.clone(),
            None => self.pending.push(details.clone()),
        }
        self.pending_requesters
            .entry(details.key.clone())
            .or_default()
            .insert(requester);
        if !matches!(self.stored_decisions, StoredDecisions::Loading) {
            if slot_was_notified && !superseded.is_empty() {
                self.notified_keys.insert(details.key.clone());
                for old in superseded {
                    cx.emit(ProjectLaunchApprovalsEvent::PendingLaunchReplaced {
                        old,
                        new: details.clone(),
                    });
                }
            } else {
                self.notify_if_new(&details, cx);
            }
        }
        cx.notify();
        details
    }

    /// `requester` no longer asks for `key`; the launch stays pending while
    /// anyone else still does.
    pub fn withdraw_pending(
        &mut self,
        key: &LaunchApprovalKey,
        requester: EntityId,
        cx: &mut Context<Self>,
    ) {
        let Some(requesters) = self.pending_requesters.get_mut(key) else {
            return;
        };
        requesters.remove(&requester);
        if !requesters.is_empty() {
            return;
        }
        let slot = self.pending_launch(key).map(LaunchSlot::of);
        self.remove_pending(key);
        if let Some(slot) = slot {
            self.free_slot_if_empty(&slot);
        }
        cx.emit(ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(
            key.clone(),
        ));
        cx.notify();
    }

    /// Everything `requester` asked for is withdrawn, e.g. when its project
    /// is closed.
    pub fn withdraw_requester(&mut self, requester: EntityId, cx: &mut Context<Self>) {
        let keys = self
            .pending_requesters
            .iter()
            .filter(|(_, requesters)| requesters.contains(&requester))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.withdraw_pending(&key, requester, cx);
        }
    }

    pub fn pending_launch_guard(
        &self,
        key: &LaunchApprovalKey,
        requester: EntityId,
    ) -> PendingLaunchGuard {
        PendingLaunchGuard {
            key: key.clone(),
            requester,
            withdrawals: self.withdrawals.clone(),
        }
    }

    /// Resolves once the user approves or denies `key`, and is canceled when
    /// the launch stops being pending some other way (e.g. its definition
    /// changed), after which the caller should look again.
    pub fn wait_for_decision(
        &mut self,
        key: &LaunchApprovalKey,
    ) -> oneshot::Receiver<LaunchDecision> {
        let (sender, receiver) = oneshot::channel();
        let waiters = self.decision_waiters.entry(key.clone()).or_default();
        waiters.retain(|waiter| !waiter.is_canceled());
        waiters.push(sender);
        receiver
    }

    /// Approves exactly `details`, as shown. Refused with
    /// [`CHANGED_SINCE_SHOWN_MESSAGE`] once a newer definition of the same
    /// launch has replaced it.
    pub fn approve(
        &mut self,
        details: LaunchApprovalDetails,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Err(error) = self.ensure_not_replaced(&details) {
            return Task::ready(Err(error));
        }
        self.record_decision(details, LaunchDecision::Approved, cx)
    }

    /// Denies exactly `details`, as shown; refused like [`Self::approve`].
    pub fn deny(
        &mut self,
        details: LaunchApprovalDetails,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Err(error) = self.ensure_not_replaced(&details) {
            return Task::ready(Err(error));
        }
        self.record_decision(details, LaunchDecision::Denied, cx)
    }

    /// Approves the pending launch keyed `key`, the one shown. Refused with
    /// [`CHANGED_SINCE_SHOWN_MESSAGE`] when it is no longer pending.
    pub fn approve_pending(
        &mut self,
        key: &LaunchApprovalKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        match self.pending_launch(key).cloned() {
            Some(details) => self.approve(details, cx),
            None => Task::ready(Err(anyhow!(CHANGED_SINCE_SHOWN_MESSAGE))),
        }
    }

    /// Denies the pending launch keyed `key`; refused like
    /// [`Self::approve_pending`].
    pub fn deny_pending(
        &mut self,
        key: &LaunchApprovalKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        match self.pending_launch(key).cloned() {
            Some(details) => self.deny(details, cx),
            None => Task::ready(Err(anyhow!(CHANGED_SINCE_SHOWN_MESSAGE))),
        }
    }

    /// Forgets the decision for `key`, the one shown; refused with
    /// [`CHANGED_SINCE_SHOWN_MESSAGE`] when there is none. Whatever asks for
    /// it next is pending again, without a new notification in this session.
    pub fn revoke(&mut self, key: &LaunchApprovalKey, cx: &mut Context<Self>) -> Task<Result<()>> {
        let key = key.clone();
        if !self.decisions.contains_key(&key) {
            return Task::ready(Err(anyhow!(CHANGED_SINCE_SHOWN_MESSAGE)));
        }
        if self
            .decisions
            .get(&key)
            .is_some_and(|record| record.session_only)
        {
            self.forget_decision(&key, cx);
            return Task::ready(Ok(()));
        }
        let store = match key_value_store(cx) {
            Ok(store) => store,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            store
                .scoped(DB_NAMESPACE)
                .delete(key.as_str().to_string())
                .await
                .context("起動の承認を取り消せませんでした")?;
            this.update(cx, |this, cx| this.forget_decision(&key, cx))
        })
    }

    /// Every decision, session-only approvals included, for a page that lets
    /// the user review them.
    pub fn decision_records(&self) -> Result<Vec<LaunchDecisionRecord>> {
        match &self.stored_decisions {
            StoredDecisions::Loading => Err(anyhow!("起動の承認を読み込んでいます")),
            StoredDecisions::Unavailable(error) => Err(anyhow!("{error}")),
            StoredDecisions::Loaded => Ok(self
                .decisions
                .values()
                .cloned()
                .sorted_by(|left, right| left.key.cmp(&right.key))
                .collect()),
        }
    }

    /// Makes every launch count as approved, without touching the database.
    #[cfg(any(test, feature = "test-support"))]
    pub fn approve_everything_for_tests(cx: &mut App) {
        let approvals = Self::global(cx);
        approvals.update(cx, |approvals, _| approvals.approve_everything = true);
    }

    /// Adds `details` as a pending launch and notifies about it the way a
    /// store asking for it would, whatever the database holds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn add_pending_launch_for_tests(
        &mut self,
        mut details: LaunchApprovalDetails,
        cx: &mut Context<Self>,
    ) {
        details.state = LaunchApprovalState::Pending;
        let requester = cx.entity_id();
        self.pending.retain(|pending| pending.key != details.key);
        self.pending.push(details.clone());
        self.pending_requesters
            .entry(details.key.clone())
            .or_default()
            .insert(requester);
        self.notify_if_new(&details, cx);
        cx.notify();
    }

    fn ensure_not_replaced(&self, details: &LaunchApprovalDetails) -> Result<()> {
        let slot = LaunchSlot::of(details);
        let replaced = self
            .pending
            .iter()
            .any(|pending| pending.key != details.key && LaunchSlot::of(pending) == slot);
        anyhow::ensure!(!replaced, CHANGED_SINCE_SHOWN_MESSAGE);
        Ok(())
    }

    fn state_of(&self, key: &LaunchApprovalKey) -> (LaunchApprovalState, Option<SharedString>) {
        #[cfg(any(test, feature = "test-support"))]
        if self.approve_everything {
            return (LaunchApprovalState::Approved, None);
        }
        if let Some(record) = self.decisions.get(key) {
            let state = match record.decision {
                LaunchDecision::Approved => LaunchApprovalState::Approved,
                LaunchDecision::Denied => LaunchApprovalState::Denied,
            };
            return (state, None);
        }
        match &self.stored_decisions {
            StoredDecisions::Loading | StoredDecisions::Loaded => {
                (LaunchApprovalState::Pending, None)
            }
            StoredDecisions::Unavailable(error) => {
                (LaunchApprovalState::Pending, Some(error.clone()))
            }
        }
    }

    fn finish_loading(&mut self, rows: Result<Vec<(String, String)>>, cx: &mut Context<Self>) {
        match rows {
            Ok(rows) => {
                for (key, value) in rows {
                    let Some(record) = serde_json::from_str::<LaunchDecisionRecord>(&value)
                        .with_context(|| format!("unreadable launch approval record {key}"))
                        .log_err()
                    else {
                        continue;
                    };
                    self.decisions
                        .entry(LaunchApprovalKey(key.into()))
                        .or_insert(record);
                }
                self.stored_decisions = StoredDecisions::Loaded;
            }
            Err(error) => {
                log::error!("could not read launch approvals: {error:#}");
                self.stored_decisions = StoredDecisions::Unavailable(format!("{error:#}").into());
            }
        }

        // Launches asked for while loading: settle the ones decided earlier,
        // and notify about the rest.
        let keys = self
            .pending
            .iter()
            .map(|pending| pending.key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            match self.state_of(&key) {
                (LaunchApprovalState::Approved, _) => {
                    self.settle(&key, LaunchDecision::Approved, cx)
                }
                (LaunchApprovalState::Denied, _) => self.settle(&key, LaunchDecision::Denied, cx),
                (LaunchApprovalState::Pending, store_error) => {
                    if let Some(pending) =
                        self.pending.iter_mut().find(|pending| pending.key == key)
                    {
                        pending.store_error = store_error;
                    }
                    if let Some(details) = self.pending_launch(&key).cloned() {
                        self.notify_if_new(&details, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    fn record_decision(
        &mut self,
        details: LaunchApprovalDetails,
        decision: LaunchDecision,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let session_only = decision == LaunchDecision::Approved && details.approval_is_session_only;
        let record = LaunchDecisionRecord::new(&details, decision, session_only);
        let key = details.key;
        if session_only {
            self.decisions.insert(key.clone(), record);
            self.settle(&key, decision, cx);
            return Task::ready(Ok(()));
        }
        let store = match key_value_store(cx) {
            Ok(store) => store,
            Err(error) => return Task::ready(Err(error)),
        };
        let value = match serde_json::to_string(&record) {
            Ok(value) => value,
            Err(error) => return Task::ready(Err(error.into())),
        };
        cx.spawn(async move |this, cx| {
            store
                .scoped(DB_NAMESPACE)
                .write(key.as_str().to_string(), value)
                .await
                .context("起動の承認を保存できませんでした")?;
            this.update(cx, |this, cx| {
                this.decisions.insert(key.clone(), record);
                this.settle(&key, decision, cx);
            })
        })
    }

    fn settle(
        &mut self,
        key: &LaunchApprovalKey,
        decision: LaunchDecision,
        cx: &mut Context<Self>,
    ) {
        for waiter in self.decision_waiters.remove(key).into_iter().flatten() {
            if waiter.send(decision).is_err() {
                log::debug!("a launch stopped waiting before its decision arrived");
            }
        }
        let slot = self.pending_launch(key).map(LaunchSlot::of);
        self.remove_pending(key);
        if let Some(slot) = slot {
            self.free_slot_if_empty(&slot);
        }
        self.notified_keys.insert(key.clone());
        cx.emit(ProjectLaunchApprovalsEvent::LaunchDecided {
            key: key.clone(),
            decision,
        });
        cx.notify();
    }

    fn forget_decision(&mut self, key: &LaunchApprovalKey, cx: &mut Context<Self>) {
        self.decisions.remove(key);
        self.notified_keys.insert(key.clone());
        cx.emit(ProjectLaunchApprovalsEvent::LaunchApprovalRevoked(
            key.clone(),
        ));
        cx.notify();
    }

    /// Anyone still waiting on `key` is canceled and looks again.
    fn remove_pending(&mut self, key: &LaunchApprovalKey) {
        self.pending.retain(|pending| &pending.key != key);
        self.pending_requesters.remove(key);
        self.decision_waiters.remove(key);
    }

    fn free_slot_if_empty(&mut self, slot: &LaunchSlot) {
        if !self
            .pending
            .iter()
            .any(|pending| &LaunchSlot::of(pending) == slot)
        {
            self.notified_slots.remove(slot);
        }
    }

    /// One live notification per launch, and never two for one definition.
    fn notify_if_new(&mut self, details: &LaunchApprovalDetails, cx: &mut Context<Self>) {
        let slot = LaunchSlot::of(details);
        if self.notified_slots.contains(&slot) || self.notified_keys.contains(&details.key) {
            return;
        }
        self.notified_slots.insert(slot);
        self.notified_keys.insert(details.key.clone());
        cx.emit(ProjectLaunchApprovalsEvent::PendingLaunchAppeared(
            details.clone(),
        ));
    }
}

fn key_value_store(cx: &App) -> Result<KeyValueStore> {
    cx.try_global::<db::AppDatabase>()
        .map(KeyValueStore::from_app_db)
        .context("起動の承認を記録するデータベースが開かれていません")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::RealFs;
    use gpui::TestAppContext;

    fn scan_of(
        program: &str,
        arguments: &[&str],
        environment: &[(&str, &str)],
        root: &Path,
    ) -> CommandLineScan {
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect::<Vec<_>>();
        CommandLineScan::new(
            Some(program),
            &arguments,
            environment.iter().copied(),
            Some(root),
            ScannedWorktrees {
                local_roots: vec![root.to_path_buf()],
                remote: Vec::new(),
            },
        )
    }

    fn real_fs(cx: &TestAppContext) -> Arc<dyn Fs> {
        Arc::new(RealFs::new(None, cx.executor()))
    }

    fn request_of(scan: CommandLineScan) -> LaunchRequest {
        LaunchRequest {
            kind: LaunchKind::ContextServer,
            name: "server".into(),
            definition: serde_json::json!({}),
            target: LaunchTarget::Url("".into()),
            environment_variable_names: Vec::new(),
            header_names: Vec::new(),
            code_loading_environment: Vec::new(),
            execution_host: ExecutionHost::ThisMachine,
            host: LaunchHost::ThisMachine,
            definition_file: LaunchDefinitionFile::Worktree("/project/.zed/settings.json".into()),
            worktree_abs_path: Path::new("/project").into(),
            replaces_user_entry: false,
            scan,
        }
    }

    fn file_name(path: &SharedString) -> String {
        Path::new(path.as_ref())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn bound_names(content: &ScannedContent) -> Vec<String> {
        content
            .bound_files
            .iter()
            .map(|(path, _)| file_name(path))
            .collect()
    }

    /// What kind each unbound entry is, by file name (a program by name).
    fn described(content: &[UnboundWorktreeContent]) -> Vec<(&'static str, String)> {
        content
            .iter()
            .map(|content| match content {
                UnboundWorktreeContent::WorkingDirectory { program, .. } => {
                    ("working-directory", program.to_string())
                }
                UnboundWorktreeContent::File(path) => ("file", file_name(path)),
                UnboundWorktreeContent::Directory(path) => ("directory", file_name(path)),
                UnboundWorktreeContent::ArgumentFile(path) => ("argument-file", file_name(path)),
                UnboundWorktreeContent::Wrapper(program) => ("wrapper", program.to_string()),
                UnboundWorktreeContent::RemoteFile(path) => ("remote-file", path.to_string()),
                UnboundWorktreeContent::Unreadable(path) => ("unreadable", file_name(path)),
            })
            .collect()
    }

    fn pending_details(key: &str, name: &str) -> LaunchApprovalDetails {
        LaunchApprovalDetails::for_tests(
            key,
            LaunchKind::ExternalFormatter,
            name,
            LaunchTarget::Command {
                program: name.to_string().into(),
                arguments: Vec::new(),
            },
        )
    }

    #[gpui::test]
    async fn test_worktree_files_are_found_however_they_are_spelled(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let root = temporary_directory.path().join("project");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts").join("serve.js"), "serve()").unwrap();
        let fs = real_fs(cx);

        let detour = Path::new("..")
            .join("project")
            .join("scripts")
            .join("serve.js");
        let mut spellings = vec![detour.to_string_lossy().into_owned()];
        #[cfg(windows)]
        {
            let absolute = root.join("scripts").join("serve.js");
            spellings.push(absolute.to_string_lossy().to_uppercase());
            spellings.push(format!(r"\\?\{}", absolute.to_string_lossy()));
        }
        for spelling in spellings {
            let content = scan_of("node", &[spelling.as_str()], &[], &root)
                .run(Some(&fs))
                .await;
            assert_eq!(
                bound_names(&content),
                vec!["serve.js"],
                "{spelling} names the script in the worktree"
            );
            assert!(!content.session_only);
        }

        let outside = temporary_directory.path().join("outside.js");
        std::fs::write(&outside, "outside()").unwrap();
        let content = scan_of("node", &[&outside.to_string_lossy()], &[], &root)
            .run(Some(&fs))
            .await;
        assert!(content.bound_files.is_empty());
        assert_eq!(
            described(&content.unbound),
            vec![("working-directory", "node".to_string())],
            "only the worktree it starts in"
        );
    }

    #[gpui::test]
    async fn test_only_the_program_and_its_script_are_bound(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let root = temporary_directory.path().join("project");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("server.js"), "serve()").unwrap();
        std::fs::write(root.join("hook.js"), "hook()").unwrap();
        std::fs::write(root.join("args.txt"), "--inspect").unwrap();
        let fs = real_fs(cx);

        let scan = scan_of(
            "env",
            &[
                "NODE_OPTIONS=--require=./hook.js",
                "node",
                "server.js",
                "@args.txt",
                "--log",
                "./server.log",
                "--data=pkg",
            ],
            &[("PYTHONPATH", ".")],
            &root,
        );
        let content = scan.run(Some(&fs)).await;
        assert_eq!(
            bound_names(&content),
            vec!["server.js"],
            "`env` runs node, whose script is server.js"
        );
        let unbound = described(&content.unbound);
        for (kind, name) in [
            ("file", "hook.js"),
            ("argument-file", "args.txt"),
            ("directory", "pkg"),
            ("directory", "project"),
            ("working-directory", "node"),
        ] {
            assert!(
                unbound.contains(&(kind, name.to_string())),
                "{kind} {name} in {unbound:?}"
            );
        }
        assert!(!content.session_only);

        // Only the script decides the key: a log file appearing does not,
        // nor does what the hook holds.
        let key = request_of(scan.clone()).resolve(Some(fs.clone())).await.key;
        std::fs::write(root.join("server.log"), "started").unwrap();
        std::fs::write(root.join("hook.js"), "changed()").unwrap();
        let details = request_of(scan.clone()).resolve(Some(fs.clone())).await;
        assert_eq!(details.key, key);
        assert!(
            described(&details.unbound_worktree_content)
                .contains(&("file", "server.log".to_string())),
            "the log file is named, but not bound"
        );
        std::fs::write(root.join("server.js"), "steal()").unwrap();
        let details = request_of(scan).resolve(Some(fs)).await;
        assert_ne!(details.key, key, "the script is bound");
    }

    #[gpui::test]
    async fn test_wrappers_modules_and_inline_code_bind_nothing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let root = temporary_directory.path().join("project");
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::create_dir_all(root.join("server")).unwrap();
        std::fs::write(root.join("server.js"), "serve()").unwrap();
        std::fs::write(root.join("tools").join("server"), "serve").unwrap();
        std::fs::write(root.join("conf.toml"), "").unwrap();
        let fs = real_fs(cx);

        for (program, arguments, wrapped) in [
            ("wsl.exe", &["-e", "node", "server.js"][..], true),
            ("direnv", &["exec", ".", "node", "server.js"][..], true),
            ("/usr/bin/env", &["-S", "node server.js"][..], true),
            ("python3", &["-m", "server"][..], false),
            ("node", &["-e", "require('./server.js')"][..], false),
            ("node", &["@server.js"][..], false),
        ] {
            let content = scan_of(program, arguments, &[], &root).run(Some(&fs)).await;
            let unbound = described(&content.unbound);
            assert!(
                content.bound_files.is_empty(),
                "{program} {arguments:?} binds nothing: {:?}",
                content.bound_files
            );
            assert_eq!(
                unbound.iter().any(|(kind, _)| *kind == "wrapper"),
                wrapped,
                "{program} {arguments:?}: {unbound:?}"
            );
            assert!(
                unbound.iter().any(|(kind, _)| *kind == "working-directory"),
                "{program} {arguments:?} starts in the worktree: {unbound:?}"
            );
        }

        let content = scan_of("tools/server", &["--config=./conf.toml"], &[], &root)
            .run(Some(&fs))
            .await;
        assert_eq!(bound_names(&content), vec!["server"]);
        assert_eq!(
            described(&content.unbound),
            vec![("file", "conf.toml".to_string())],
            "a program of the worktree is bound, so where it starts is no warning"
        );
    }

    #[test]
    fn test_the_script_is_the_first_argument_that_is_no_option() {
        let script = |program: &str, arguments: &[&str]| {
            let arguments = arguments
                .iter()
                .map(|argument| argument.to_string())
                .collect::<Vec<_>>();
            parse_command_line(Some(program), &arguments).script
        };
        assert_eq!(
            script("node", &["--require", "./hook.js", "server.js"]).as_deref(),
            Some("server.js")
        );
        assert_eq!(
            script("uv", &["run", "--with", "httpx", "server.py"]).as_deref(),
            Some("server.py")
        );
        assert_eq!(
            script(
                "pwsh",
                &[
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    "run.ps1"
                ]
            )
            .as_deref(),
            Some("run.ps1")
        );
        assert_eq!(
            script("C:\\Python312\\python.exe", &["-u", "server.py"]).as_deref(),
            Some("server.py")
        );
        assert_eq!(script("python3", &["-m", "server"]), None);
        assert_eq!(script("cmd.exe", &["/c", "run.bat"]), None);
        assert_eq!(
            script("npx", &["-y", "@modelcontextprotocol/server-filesystem"]),
            None
        );

        let arguments = ["-u", "HOME", "A=1", "node", "x.js"].map(String::from);
        let parts = parse_command_line(Some("env"), &arguments);
        assert_eq!(parts.program.as_deref(), Some("node"));
        assert_eq!(parts.script.as_deref(), Some("x.js"));
    }

    fn script_of(program: &str, arguments: &[&str]) -> Option<String> {
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect::<Vec<_>>();
        parse_command_line(Some(program), &arguments).script
    }

    #[test]
    fn test_each_runner_has_its_own_options_and_scripts() {
        assert_eq!(
            script_of("deno", &["run", "-A", "--config", "./c.json", "./main.ts"]).as_deref(),
            Some("./main.ts")
        );
        assert_eq!(
            script_of("deno", &["run", "--lock", "deno.lock", "main.ts"]).as_deref(),
            Some("main.ts")
        );
        assert_eq!(
            script_of("deno", &["run", "--unknown-flag", "./c.json", "./main.ts"]),
            None,
            "a config file an unknown option took is never the script"
        );
        assert_eq!(script_of("deno", &["eval", "console.log(1)"]), None);
        assert_eq!(script_of("deno", &["task", "start"]), None);
        assert_eq!(
            script_of("bun", &["run", "--preload", "./setup.ts", "./server.ts"]).as_deref(),
            Some("./server.ts")
        );
        assert_eq!(
            script_of("bun", &["--config", "./bunfig.toml", "server.ts"]).as_deref(),
            Some("server.ts")
        );
        assert_eq!(script_of("bun", &["run", "build"]), None);
        assert_eq!(
            script_of("uv", &["run", "-p", "3.12", "server.py"]).as_deref(),
            Some("server.py")
        );
        assert_eq!(script_of("uv", &["run", "uvicorn", "app:app"]), None);
        assert_eq!(script_of("npx", &["-c", "node x.js"]), None);
        assert_eq!(
            script_of("node", &["--env-file", ".env", "server.js"]).as_deref(),
            Some("server.js")
        );
        assert_eq!(
            script_of("node", &["--unknown", "settings.json", "server.js"]),
            None
        );
    }

    // A runner that is itself a worktree file covers nothing more when what
    // it runs could not be bound: the warnings stay.
    #[gpui::test]
    async fn test_a_bound_runner_covers_only_a_bound_script(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let root = temporary_directory.path().join("project");
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::write(root.join("tools").join("deno"), "deno").unwrap();
        std::fs::write(root.join("c.json"), "{}").unwrap();
        std::fs::write(root.join("main.ts"), "serve()").unwrap();
        let fs = real_fs(cx);

        let content = scan_of(
            "./tools/deno",
            &["run", "-A", "--unknown", "./c.json", "./main.ts"],
            &[],
            &root,
        )
        .run(Some(&fs))
        .await;
        assert_eq!(bound_names(&content), vec!["deno"]);
        let unbound = described(&content.unbound);
        for (kind, name) in [
            ("working-directory", "./tools/deno"),
            ("file", "c.json"),
            ("file", "main.ts"),
        ] {
            assert!(
                unbound.contains(&(kind, name.to_string())),
                "{kind} {name} in {unbound:?}"
            );
        }

        let content = scan_of(
            "./tools/deno",
            &["run", "-A", "--config", "./c.json", "./main.ts"],
            &[],
            &root,
        )
        .run(Some(&fs))
        .await;
        assert_eq!(bound_names(&content), vec!["deno", "main.ts"]);
        assert_eq!(
            described(&content.unbound),
            vec![("file", "c.json".to_string())]
        );
    }

    #[test]
    fn test_a_file_replaced_while_being_opened_is_refused() {
        let before = FileIdentity {
            device: 1,
            index: 7,
        };
        assert!(ensure_same_file(before, before).is_ok());
        assert!(
            ensure_same_file(
                before,
                FileIdentity {
                    device: 1,
                    index: 8
                }
            )
            .is_err()
        );
        assert!(
            ensure_same_file(
                before,
                FileIdentity {
                    device: 2,
                    index: 7
                }
            )
            .is_err()
        );

        let temporary_directory = tempfile::tempdir().unwrap();
        let file = temporary_directory.path().join("server.js");
        std::fs::write(&file, "serve()").unwrap();
        assert!(hash_regular_file(&file, MAX_BOUND_FILE_SIZE).is_ok());
        #[cfg(windows)]
        {
            assert_eq!(
                windows_identity_before_opening(&file).unwrap(),
                windows_file_identity(&std::fs::File::open(&file).unwrap()).unwrap(),
                "the same file through either handle"
            );
            let link = temporary_directory.path().join("link.js");
            // Creating a symbolic link takes a privilege or developer mode.
            if std::os::windows::fs::symlink_file(&file, &link).is_ok() {
                let error = hash_regular_file(&link, MAX_BOUND_FILE_SIZE).unwrap_err();
                assert!(error.to_string().contains("link"), "{error:#}");
            }
        }
    }

    #[gpui::test]
    async fn test_a_script_that_cannot_be_read_is_unbound_and_session_only(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let root = temporary_directory.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("locked.js");
        std::fs::write(&script, "serve()").unwrap();
        let fs = real_fs(cx);

        #[cfg(windows)]
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&script)
                .unwrap()
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        // Whoever can read it anyway (root on Unix) has nothing to test.
        if std::fs::File::open(&script).is_ok() {
            return;
        }
        let content = scan_of("node", &["locked.js"], &[], &root)
            .run(Some(&fs))
            .await;
        assert!(content.bound_files.is_empty());
        assert!(content.session_only);
        assert!(
            described(&content.unbound).contains(&("unreadable", "locked.js".to_string())),
            "{:?}",
            content.unbound
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_a_fifo_is_never_waited_on(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let root = temporary_directory.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let fifo = root.join("server.js");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let fs = real_fs(cx);

        let content = scan_of("node", &["server.js"], &[], &root)
            .run(Some(&fs))
            .await;
        assert!(content.bound_files.is_empty());
        assert!(content.session_only);
        assert!(
            described(&content.unbound).contains(&("unreadable", "server.js".to_string())),
            "{:?}",
            content.unbound
        );
        assert!(hash_regular_file(&fifo, MAX_BOUND_FILE_SIZE).is_err());
    }

    #[test]
    fn test_files_beyond_the_size_limit_are_not_read() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let file = temporary_directory.path().join("big.bin");
        std::fs::write(&file, b"12345").unwrap();
        assert!(hash_regular_file(&file, 5).is_ok());
        assert!(
            hash_regular_file(&file, 4)
                .unwrap_err()
                .to_string()
                .contains("larger than")
        );
        assert!(
            hash_regular_file(temporary_directory.path(), 5).is_err(),
            "a directory is not a regular file"
        );
    }

    #[gpui::test]
    async fn test_a_worktree_whose_identity_cannot_be_read_is_compared_by_spelling(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().unwrap();
        let fs = real_fs(cx);

        let missing_root = temporary_directory.path().join("gone");
        let content = scan_of("node", &["server.js"], &[], &missing_root)
            .run(Some(&fs))
            .await;
        assert!(
            content.session_only,
            "a path spelled differently into it could not be told apart"
        );
        assert!(described(&content.unbound).contains(&("unreadable", "gone".to_string())));

        // Without a file system every root is compared by spelling.
        let root = temporary_directory.path().join("project");
        let content = scan_of("node", &["./lib/../server.js"], &[], &root)
            .run(None)
            .await;
        assert!(content.session_only);
        assert!(
            described(&content.unbound).contains(&("unreadable", "server.js".to_string())),
            "{:?}",
            content.unbound
        );

        assert!(is_inside_by_spelling(
            &root.join("a").join("..").join("b"),
            &root
        ));
        assert!(!is_inside_by_spelling(
            &temporary_directory.path().join("project-other").join("x"),
            &root
        ));
        assert!(!is_inside_by_spelling(
            &root.join("..").join("outside"),
            &root
        ));
        #[cfg(windows)]
        assert!(is_inside_by_spelling(
            Path::new(r"C:\Project\Sub\x.js"),
            Path::new(r"\\?\c:\project")
        ));
    }

    #[gpui::test]
    async fn test_paths_into_a_remote_worktree_count_without_being_loaded(
        _cx: &mut TestAppContext,
    ) {
        let snapshot = Snapshot::new(
            worktree::WorktreeId::from_usize(1),
            RelPath::empty_arc(),
            Path::new("/remote/project").into(),
            PathStyle::Unix,
        );
        let arguments = [
            "node_modules/.bin/server.js",
            "--log",
            "./server.log",
            "-y",
            "https://example.com/a/b",
            "@scope/pkg",
            "../outside/x.js",
        ]
        .map(String::from);
        let scan = CommandLineScan::new(
            Some("node"),
            &arguments,
            [("NODE_OPTIONS", "--require=./hook.js")],
            Some(Path::new("/remote/project")),
            ScannedWorktrees {
                local_roots: Vec::new(),
                remote: vec![snapshot],
            },
        );
        let content = scan.run(None).await;
        assert!(
            content.session_only,
            "the script over there cannot be read from here"
        );
        let remote_files = content
            .unbound
            .iter()
            .filter_map(|content| match content {
                UnboundWorktreeContent::RemoteFile(path) => Some(path.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            remote_files,
            vec![
                "/remote/project/node_modules/.bin/server.js",
                "/remote/project/server.log",
                "/remote/project/hook.js",
            ],
            "not loaded, yet spelled into the worktree; options, URLs, package \
             names and paths out of it are not"
        );
    }

    #[test]
    fn test_only_code_loading_environment_values_are_shown() {
        let shown = code_loading_environment([
            ("API_TOKEN", "secret"),
            ("Path", "C:\\bin"),
            ("NODE_OPTIONS", "--require=./x.js"),
            ("DYLD_INSERT_LIBRARIES", "/tmp/x.dylib"),
            ("JAVA_TOOL_OPTIONS", "-javaagent:x.jar"),
            ("GIT_SSH_COMMAND", "ssh -i key"),
        ]);
        assert_eq!(
            shown,
            vec![
                ("DYLD_INSERT_LIBRARIES".into(), "/tmp/x.dylib".into()),
                ("GIT_SSH_COMMAND".into(), "ssh -i key".into()),
                ("JAVA_TOOL_OPTIONS".into(), "-javaagent:x.jar".into()),
                ("NODE_OPTIONS".into(), "--require=./x.js".into()),
                ("Path".into(), "C:\\bin".into()),
            ]
        );
    }

    #[gpui::test]
    fn test_launch_gate_blocks_without_anyone_to_ask(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(matches!(launch_gate(None, cx), LaunchGate::Unavailable));
            cx.set_global(db::AppDatabase::test_new());
            init(cx);
            assert!(matches!(
                launch_gate(None, cx),
                LaunchGate::Ask(LaunchApprover::Local(_))
            ));
            ProjectLaunchApprovals::mark_process_headless(cx);
            assert!(
                matches!(launch_gate(None, cx), LaunchGate::Unavailable),
                "the remote server never decides by itself, and without an IDE nothing runs"
            );
        });
    }

    #[gpui::test]
    async fn test_a_decision_taken_back_is_no_longer_approved(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let approvals = cx.update(ProjectLaunchApprovals::global);
        cx.run_until_parked();
        let approver = LaunchApprover::Local(approvals.clone());
        let details = pending_details("taken-back", "fmt");
        let key = details.key.clone();
        assert!(!cx.update(|cx| approver.still_approved(&key, cx)));

        approvals
            .update(cx, |approvals, cx| approvals.approve(details, cx))
            .await
            .unwrap();
        assert!(cx.update(|cx| approver.still_approved(&key, cx)));
        approvals
            .update(cx, |approvals, cx| approvals.revoke(&key, cx))
            .await
            .unwrap();
        assert!(
            !cx.update(|cx| approver.still_approved(&key, cx)),
            "a start that was on its way stops here"
        );
    }

    #[test]
    fn test_ssh_connections_through_other_hops_are_other_hosts() {
        let identity = |command: &str| {
            let options = remote::SshConnectionOptions::parse_command_line(command).unwrap();
            LaunchHost::from_remote_connection(&RemoteConnectionOptions::Ssh(options))
        };
        assert_eq!(identity("dev@build"), identity("dev@build"));
        assert_ne!(
            identity("dev@build -J jump-a"),
            identity("dev@build -J jump-b")
        );
        assert_ne!(identity("dev@build"), identity("dev@build -J jump-a"));
        assert_ne!(identity("dev@build"), identity("dev@build -p 2222"));
        let LaunchHost::Remote {
            identity: stored, ..
        } = identity("dev@build -J jump-a")
        else {
            panic!("an SSH connection is a remote host");
        };
        assert!(
            !stored.contains("jump-a"),
            "only a digest of the arguments is stored with decisions: {stored}"
        );
    }

    #[gpui::test]
    async fn test_pending_launches_are_shared_superseded_and_notified_once(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let approvals = cx.update(ProjectLaunchApprovals::global);
        cx.run_until_parked();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&approvals, move |_, event, _| {
                events.borrow_mut().push(event.clone())
            })
        });
        let first_window = cx.new(|_| ()).entity_id();
        let second_window = cx.new(|_| ()).entity_id();

        approvals.update(cx, |approvals, cx| {
            approvals.request_launch(pending_details("first", "fmt"), first_window, cx);
            approvals.request_launch(pending_details("first", "fmt"), second_window, cx);
            approvals.withdraw_pending(&LaunchApprovalKey::for_tests("first"), first_window, cx);
            assert_eq!(
                approvals.pending_launches().len(),
                1,
                "another window still asks for it"
            );

            approvals.request_launch(pending_details("second", "fmt"), first_window, cx);
            assert_eq!(
                approvals
                    .pending_launches()
                    .iter()
                    .map(|pending| pending.key.as_str())
                    .collect::<Vec<_>>(),
                vec!["second"],
                "a newer definition of the same launch replaces the older one"
            );
        });
        assert!(
            matches!(
                events.borrow().as_slice(),
                [
                    ProjectLaunchApprovalsEvent::PendingLaunchAppeared(first),
                    ProjectLaunchApprovalsEvent::PendingLaunchReplaced { old, new },
                ] if first.key.as_str() == "first"
                    && old.as_str() == "first"
                    && new.key.as_str() == "second"
            ),
            "one notification, updated in place: {:?}",
            events.borrow()
        );

        // Only what is on screen can be decided.
        let replaced = approvals
            .update(cx, |approvals, cx| {
                approvals.approve(pending_details("first", "fmt"), cx)
            })
            .await;
        assert_eq!(
            replaced.expect_err("a replaced definition").to_string(),
            CHANGED_SINCE_SHOWN_MESSAGE
        );
        let gone = approvals
            .update(cx, |approvals, cx| {
                approvals.approve_pending(&LaunchApprovalKey::for_tests("first"), cx)
            })
            .await;
        assert_eq!(
            gone.expect_err("no longer pending").to_string(),
            CHANGED_SINCE_SHOWN_MESSAGE
        );

        approvals.update(cx, |approvals, cx| {
            approvals.withdraw_pending(&LaunchApprovalKey::for_tests("second"), first_window, cx);
            assert!(approvals.pending_launches().is_empty());
        });
        assert!(matches!(
            events.borrow().last(),
            Some(ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(key)) if key.as_str() == "second"
        ));
    }

    #[test]
    fn test_command_lines_show_what_runs() {
        let target = LaunchTarget::Command {
            program: "I:\\FITELnet-mcp\\.venv\\Scripts\\fitelnet-mcp.exe".into(),
            arguments: vec![
                "say \"hi\"".into(),
                "\u{202E}sj.exe".into(),
                "C:\\tools\\x.js".into(),
                "".into(),
            ],
        };
        assert_eq!(
            target.display().to_string(),
            "I:\\FITELnet-mcp\\.venv\\Scripts\\fitelnet-mcp.exe \"say \"hi\"\" <U+202E>sj.exe C:\\tools\\x.js \"\"",
            "paths read exactly as they run, and nothing invisible stays invisible"
        );
        assert_eq!(
            escape_for_display("a\u{200B}b\u{E0041}"),
            "a<U+200B>b<U+E0041>"
        );
        assert_eq!(escape_for_display("C:\\plain\\path"), "C:\\plain\\path");
    }

    #[gpui::test]
    async fn test_session_only_approvals_are_not_stored(cx: &mut TestAppContext) {
        let database = db::AppDatabase::test_new();
        let store = KeyValueStore::from_app_db(&database);
        cx.update(|cx| cx.set_global(database));
        let approvals = cx.update(ProjectLaunchApprovals::global);
        cx.run_until_parked();

        let mut details = pending_details("remote-script", "server");
        details.approval_is_session_only = true;
        approvals
            .update(cx, |approvals, cx| approvals.approve(details, cx))
            .await
            .unwrap();

        let records = approvals.read_with(cx, |approvals, _| approvals.decision_records().unwrap());
        assert_eq!(records.len(), 1);
        assert!(records[0].session_only);
        assert_eq!(
            store.scoped(DB_NAMESPACE).read("remote-script").unwrap(),
            None
        );
    }
}
