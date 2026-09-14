use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub use darwin::{Child, Command, Stdio};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

pub use gpui_util::new_std_command;

/// ADR-F50: set to anything in a child's environment, this makes Windows
/// (`CreateProcess`, `cmd.exe`) stop looking for a bare program name in the
/// child's working directory, which for a language server, a formatter or an
/// MCP server is a project folder: a `.cmd` shim that runs `node` must not
/// find a `node.exe` the project put there.
pub const NO_CURRENT_DIRECTORY_PROGRAM_LOOKUP: &str = "NoDefaultCurrentDirectoryInExePath";

pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

/// Resolves a bare external-command name (e.g. `"clang-format"`, as configured by
/// `languages.<lang>.formatter.external.command`) to a binary bundled next to the
/// running executable, so a shipped copy can take priority over whatever (if
/// anything) is found on `PATH`.
///
/// Returns `None` — meaning "leave `command` untouched and let the normal `PATH`
/// lookup happen exactly as before" — when either:
/// - `command` is not a bare name (it contains a path separator, or is already
///   absolute). Such commands are meant literally by whoever configured them and
///   must never be redirected.
/// - no matching binary exists next to the executable or in its `bin` subdirectory.
///
/// This is a general-purpose, command-name-agnostic resolver (it has no
/// knowledge of `clang-format` specifically); callers opt into it explicitly
/// rather than it being wired into `new_command` globally, since `new_command`
/// backs unrelated things (shells, LSP servers, DAP adapters, git, node, ssh)
/// where silently preferring a same-named binary shipped next to Zed would be
/// surprising.
pub fn resolve_command_next_to_current_exe(command: &str) -> Option<PathBuf> {
    resolve_bare_command_in_dirs(command, &command_search_dirs_next_to_current_exe())
}

/// The directories [`resolve_command_next_to_current_exe`] looks in, in order.
/// Exposed so a "command not found" message can name them instead of leaving
/// the user to guess where a bundled binary is supposed to go.
pub fn command_search_dirs_next_to_current_exe() -> Vec<PathBuf> {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return Vec::new();
    };
    let bin_dir = exe_dir.join("bin");
    vec![exe_dir, bin_dir]
}

fn resolve_bare_command_in_dirs(command: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() || path.file_name() != Some(OsStr::new(command)) {
        return None;
    }

    let mut candidate_names = vec![command.to_owned()];
    if cfg!(target_os = "windows") && path.extension().is_none() {
        candidate_names.push(format!("{command}.exe"));
    }

    dirs.iter().find_map(|dir| {
        candidate_names
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod resolve_command_tests {
    use super::*;

    fn bundled_name() -> &'static str {
        if cfg!(target_os = "windows") {
            "clang-format.exe"
        } else {
            "clang-format"
        }
    }

    #[test]
    fn prefers_binary_next_to_exe_over_path() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join(bundled_name());
        std::fs::write(&bundled, b"").unwrap();

        let resolved = resolve_bare_command_in_dirs("clang-format", &[dir.path().to_path_buf()]);
        assert_eq!(resolved.as_deref(), Some(bundled.as_path()));
    }

    #[test]
    fn falls_back_to_path_when_nothing_bundled() {
        let dir = tempfile::tempdir().unwrap();

        let resolved = resolve_bare_command_in_dirs("clang-format", &[dir.path().to_path_buf()]);
        assert_eq!(resolved, None);
    }

    #[test]
    fn absolute_or_path_qualified_commands_pass_through_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join(bundled_name());
        std::fs::write(&bundled, b"").unwrap();

        // Even though a same-named binary exists in the search dir, a command
        // that already names a directory (relative or absolute) must never be
        // redirected to a different path than the one that was configured.
        let relative_qualified = format!(".{}clang-format", std::path::MAIN_SEPARATOR);
        assert_eq!(
            resolve_bare_command_in_dirs(&relative_qualified, &[dir.path().to_path_buf()]),
            None
        );
        assert_eq!(
            resolve_bare_command_in_dirs(bundled.to_str().unwrap(), &[dir.path().to_path_buf()]),
            None
        );
    }

    #[test]
    fn checks_bin_subdirectory_too() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let bundled = bin_dir.join(bundled_name());
        std::fs::write(&bundled, b"").unwrap();

        let resolved =
            resolve_bare_command_in_dirs("clang-format", &[bin_dir, dir.path().to_path_buf()]);
        assert_eq!(resolved.as_deref(), Some(bundled.as_path()));
    }
}

#[cfg(not(target_os = "macos"))]
pub type Child = smol::process::Child;

#[cfg(not(target_os = "macos"))]
pub use std::process::Stdio;

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct Command(smol::process::Command);

#[cfg(not(target_os = "macos"))]
impl Command {
    #[inline]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        #[cfg(target_os = "windows")]
        {
            use smol::process::windows::CommandExt;
            let mut cmd = smol::process::Command::new(program);
            cmd.creation_flags(CREATE_NO_WINDOW);
            Self(cmd)
        }
        #[cfg(not(target_os = "windows"))]
        Self(smol::process::Command::new(program))
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.0.get_args()
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdin(cfg.into());
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdout(cfg.into());
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stderr(cfg.into());
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.0.kill_on_drop(kill_on_drop);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Child> {
        self.0.spawn()
    }

    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.0.output().await
    }

    pub async fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.0.get_program()
    }
}
