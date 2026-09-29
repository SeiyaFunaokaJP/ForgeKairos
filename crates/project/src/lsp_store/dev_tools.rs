//! What a language needs on the machine that owns the files, and how to put it
//! there without root (ADR-F39).
//!
//! Everything in here runs on that machine: locally for a local project, inside
//! `remote_server` for a remote one. That is what makes an offline SSH host
//! work — the remote's HTTP goes back out through the client (ADR-F27), so the
//! bytes come from the client's network and its allowlist, while the install
//! itself lands in the remote user's own data directory. Nothing is written
//! outside `$HOME`, nothing needs `sudo`, and no other account on the host is
//! affected.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use http_client::{HttpClient, github::AssetKind, github_download::download_server_binary};
use rpc::{ErrorCode, ErrorCodeExt as _, ErrorExt as _};
use serde::{Deserialize, Serialize};

/// Why a tool is wanted, in the words the notification uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevToolPurpose {
    Diagnostics,
    Formatting,
    TypeCheck,
}

impl DevToolPurpose {
    pub fn label(&self) -> &'static str {
        match self {
            DevToolPurpose::Diagnostics => i18n::tc!("dev tool purpose", "Diagnostics"),
            DevToolPurpose::Formatting => i18n::tc!("dev tool purpose", "Formatting"),
            DevToolPurpose::TypeCheck => i18n::tc!("dev tool purpose", "Type Checking"),
        }
    }
}

/// How a missing tool can be obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevToolAcquisition {
    /// A language server Zed already knows how to download. Installing it means
    /// letting Zed's own fetch run again, so none of that machinery (versions,
    /// asset names, unpack layout) is duplicated here.
    LanguageServer,
    /// A standalone binary Zed never fetches on its own, published as a Python
    /// wheel. PyPI is used because its metadata carries a SHA-256 for every
    /// file, which is the only redistribution of these binaries that lets the
    /// download be verified.
    PyPiWheel {
        project: &'static str,
        version: &'static str,
    },
    /// Needs a package manager (and usually root), so there is nothing to
    /// offer but an explanation.
    Guidance(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct DevTool {
    /// Stable identifier used on the wire and in `dev_tools.ignored`.
    pub id: &'static str,
    /// The command looked for on the owning machine, and the file name an
    /// install writes into [`paths::managed_tools_dir`].
    pub command: &'static str,
    pub display_name: &'static str,
    pub purpose: DevToolPurpose,
    /// Language names as `LanguageName` spells them (`"C++"`, not `"cpp"`).
    pub languages: &'static [&'static str],
    pub acquisition: DevToolAcquisition,
}

impl DevTool {
    /// The language server whose download this tool is, when it is one. Also
    /// the directory Zed unpacks that server into, which is how an already
    /// downloaded server is recognized without asking the LSP store.
    pub fn language_server_name(&self) -> Option<&'static str> {
        match self.acquisition {
            DevToolAcquisition::LanguageServer => Some(self.id),
            _ => None,
        }
    }

    pub fn can_install(&self) -> bool {
        !matches!(self.acquisition, DevToolAcquisition::Guidance(_))
    }
}

/// Only tools that are worth interrupting someone over: a missing one means the
/// language visibly does less (no diagnostics, no formatting), and there is
/// something honest to say about it. Compilers and interpreters are listed so
/// the notification can explain itself, not because they can be installed.
static CATALOG: &[DevTool] = &[
    DevTool {
        id: "clangd",
        command: "clangd",
        display_name: "clangd",
        purpose: DevToolPurpose::Diagnostics,
        languages: &["C", "C++"],
        acquisition: DevToolAcquisition::LanguageServer,
    },
    DevTool {
        id: "clang-format",
        command: "clang-format",
        display_name: "clang-format",
        purpose: DevToolPurpose::Formatting,
        languages: &["C", "C++"],
        acquisition: DevToolAcquisition::PyPiWheel {
            project: "clang-format",
            version: "20.1.7",
        },
    },
    DevTool {
        id: "ruff",
        command: "ruff",
        display_name: "Ruff",
        purpose: DevToolPurpose::Diagnostics,
        languages: &["Python"],
        acquisition: DevToolAcquisition::LanguageServer,
    },
    DevTool {
        id: "rust-analyzer",
        command: "rust-analyzer",
        display_name: "rust-analyzer",
        purpose: DevToolPurpose::Diagnostics,
        languages: &["Rust"],
        acquisition: DevToolAcquisition::LanguageServer,
    },
    DevTool {
        id: "go",
        command: "go",
        display_name: i18n::msg!("Go toolchain"),
        purpose: DevToolPurpose::Diagnostics,
        languages: &["Go"],
        acquisition: DevToolAcquisition::Guidance(i18n::msg!(
            "gopls (the Go language server) is built with the Go toolchain, so go is needed first. \
             Ask an administrator, or unpack the official distribution and put it on PATH."
        )),
    },
];

pub fn catalog() -> &'static [DevTool] {
    CATALOG
}

pub fn tool_by_id(id: &str) -> Option<&'static DevTool> {
    CATALOG.iter().find(|tool| tool.id == id)
}

pub fn tool_by_command(command: &str) -> Option<&'static DevTool> {
    CATALOG.iter().find(|tool| tool.command == command)
}

pub fn tools_for_language(language: &str) -> impl Iterator<Item = &'static DevTool> + use<'_> {
    CATALOG
        .iter()
        .filter(move |tool| tool.languages.contains(&language))
}

/// A tool this application installed, if it is there. Bare names only: a
/// configured command with a path in it was meant literally (the same rule
/// [`util::command::resolve_command_next_to_current_exe`] follows).
pub fn resolve_managed_tool(command: &str) -> Option<PathBuf> {
    if Path::new(command).file_name() != Some(command.as_ref()) {
        return None;
    }
    let path = paths::managed_tools_dir().join(executable_file_name(command));
    path.is_file().then_some(path)
}

fn executable_file_name(command: &str) -> String {
    if cfg!(target_os = "windows") && !command.ends_with(".exe") {
        format!("{command}.exe")
    } else {
        command.to_string()
    }
}

/// The [`rpc::RpcError`] tag an egress refusal rides on, so that an install
/// that ran on a remote host still reaches the ADR-F22 consent flow on the
/// client. The RPC boundary flattens `anyhow` chains into one string, which
/// would otherwise leave nothing to downcast.
const EGRESS_DENIED_TAG: &str = "dev_tool_egress_hosts";

/// Re-raises an install failure so that the hosts an allowlist refused survive
/// the trip to the client. Anything else is passed through untouched.
pub fn tag_egress_denial(error: anyhow::Error) -> anyhow::Error {
    let Some(hosts) = http_client::egress_denied_hosts(&error) else {
        return error;
    };
    ErrorCode::Internal
        .message(format!("{error:#}"))
        .with_tag(EGRESS_DENIED_TAG, &hosts.join(","))
        .anyhow()
}

/// The hosts whose refusal made an install fail, whether it failed here or on
/// the other side of an RPC.
pub fn egress_denied_hosts(error: &anyhow::Error) -> Option<Vec<String>> {
    if let Some(hosts) = http_client::egress_denied_hosts(error) {
        return Some(hosts);
    }
    let hosts = error
        .error_tag(EGRESS_DENIED_TAG)?
        .split(',')
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!hosts.is_empty()).then_some(hosts)
}

/// Where a tool was found, so the notification can say why it is not asking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevToolState {
    pub id: String,
    pub present: bool,
    /// The resolved path when it is known. Absent for a language server that
    /// Zed has already downloaded but that is not on `PATH`.
    pub path: Option<String>,
}

/// Checks a language's tools on this machine.
///
/// `path_env` and `working_directory` are the `PATH` and directory a language
/// server would be looked up with, so that a tool installed only inside the
/// project's environment (a virtualenv, a direnv) counts as present.
pub async fn check_tools(
    language: &str,
    path_env: Option<String>,
    working_directory: PathBuf,
) -> Vec<DevToolState> {
    // A single-file worktree's root is the file itself, and `which_in` wants a
    // directory to resolve a relative command against.
    let working_directory = if working_directory.is_file() {
        working_directory
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(working_directory)
    } else {
        working_directory
    };
    let mut states = Vec::new();
    for tool in tools_for_language(language) {
        let found = which::which_in(
            OsString::from(tool.command),
            path_env.as_ref(),
            &working_directory,
        )
        .ok()
        .or_else(|| resolve_managed_tool(tool.command));
        let downloaded_by_zed = found.is_none()
            && tool
                .language_server_name()
                .is_some_and(language_server_is_downloaded);
        states.push(DevToolState {
            id: tool.id.to_string(),
            present: found.is_some() || downloaded_by_zed,
            path: found.map(|path| path.to_string_lossy().into_owned()),
        });
    }
    states
}

/// Whether Zed has unpacked this server before. A previous download is enough
/// to stay quiet: the fetch is skipped from then on, so a missing binary would
/// surface as a start-up failure (ADR-F22) rather than as an absent tool.
fn language_server_is_downloaded(server_name: &str) -> bool {
    let dir = paths::languages_dir().join(server_name);
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Downloads a tool and puts it in [`paths::managed_tools_dir`], returning its
/// path. Only for [`DevToolAcquisition::PyPiWheel`]; a language server is
/// installed by letting Zed fetch it again, which the caller drives.
pub async fn install_tool(tool: &DevTool, http_client: Arc<dyn HttpClient>) -> Result<PathBuf> {
    let DevToolAcquisition::PyPiWheel { project, version } = tool.acquisition else {
        anyhow::bail!(i18n::t!(
            "{tool} is not a tool that can be downloaded and installed",
            tool = tool.id
        ));
    };

    let files = fetch_pypi_release(&*http_client, project, version).await?;
    let wheel =
        select_wheel(&files, std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
            i18n::t!(
                "{project} {version} has no distribution for this environment ({os} {arch})",
                os = std::env::consts::OS,
                arch = std::env::consts::ARCH
            )
        })?;

    let tools_dir = paths::managed_tools_dir();
    smol::fs::create_dir_all(tools_dir).await.with_context(|| {
        i18n::t!(
            "Could not create the directory to install tools in: {path}",
            path = format!("{tools_dir:?}")
        )
    })?;

    // Unpacked beside the destination rather than in a system temporary
    // directory: the two have to be on the same filesystem for the final move,
    // and a full disk should fail here rather than halfway through.
    let unpack_dir = tools_dir.join(format!(".{}-unpack", tool.id));
    remove_dir_if_present(&unpack_dir).await;
    let result = install_from_wheel(tool, &wheel, &*http_client, &unpack_dir, tools_dir).await;
    remove_dir_if_present(&unpack_dir).await;
    result
}

async fn install_from_wheel(
    tool: &DevTool,
    wheel: &PyPiFile,
    http_client: &dyn HttpClient,
    unpack_dir: &Path,
    tools_dir: &Path,
) -> Result<PathBuf> {
    download_server_binary(
        http_client,
        &wheel.url,
        wheel.digests.sha256.as_deref(),
        unpack_dir,
        AssetKind::Zip,
    )
    .await
    .with_context(|| {
        i18n::t!(
            "Could not download {tool}",
            tool = i18n::tr(tool.display_name)
        )
    })?;

    let file_name = executable_file_name(tool.command);
    let extracted = find_file_in_tree(unpack_dir, &file_name).with_context(|| {
        i18n::t!(
            "The downloaded distribution did not contain {file_name} ({archive})",
            archive = wheel.filename
        )
    })?;

    let destination = tools_dir.join(&file_name);
    // `rename` fails across devices, and a running copy of the tool cannot be
    // overwritten on Windows; both are worth reporting as themselves rather
    // than as a bare io error.
    smol::fs::rename(&extracted, &destination)
        .await
        .with_context(|| {
            i18n::t!(
                "Could not move {file_name} to {destination}",
                destination = format!("{destination:?}")
            )
        })?;
    util::fs::make_file_executable(&destination)
        .await
        .with_context(|| {
            i18n::t!(
                "Could not make {destination} executable",
                destination = format!("{destination:?}")
            )
        })?;
    Ok(destination)
}

async fn remove_dir_if_present(dir: &Path) {
    if let Err(error) = smol::fs::remove_dir_all(dir).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("failed to clear the dev tool staging directory {dir:?}: {error}");
    }
}

fn find_file_in_tree(root: &Path, file_name: &str) -> Option<PathBuf> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        // A directory that cannot be read is skipped rather than ending the
        // search: the file being looked for may well be in a sibling.
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => directories.push(path),
                Ok(_) if path.file_name() == Some(file_name.as_ref()) => return Some(path),
                _ => {}
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct PyPiRelease {
    urls: Vec<PyPiFile>,
}

#[derive(Debug, Deserialize)]
struct PyPiFile {
    filename: String,
    url: String,
    packagetype: String,
    #[serde(default)]
    yanked: bool,
    digests: PyPiDigests,
}

#[derive(Debug, Default, Deserialize)]
struct PyPiDigests {
    sha256: Option<String>,
}

async fn fetch_pypi_release(
    http_client: &dyn HttpClient,
    project: &str,
    version: &str,
) -> Result<Vec<PyPiFile>> {
    let url = format!("https://pypi.org/pypi/{project}/{version}/json");
    let mut response = http_client
        .get(&url, Default::default(), true)
        .await
        .with_context(|| {
            i18n::t!("Could not fetch the release information of {project} {version}")
        })?;
    anyhow::ensure!(
        response.status().is_success(),
        i18n::t!(
            "Could not fetch the release information of {project} {version} (HTTP {status})",
            status = response.status()
        )
    );
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .with_context(|| {
            i18n::t!("Could not read the release information of {project} {version}")
        })?;
    let release: PyPiRelease = serde_json::from_str(&body).with_context(|| {
        i18n::t!("Could not parse the release information of {project} {version}")
    })?;
    Ok(release.urls)
}

/// Picks the wheel built for this machine. The tags are matched as substrings
/// of the file name because that is what the wheel naming convention makes
/// stable; an exact tag would break on every new manylinux baseline.
fn select_wheel<'a>(files: &'a [PyPiFile], os: &str, arch: &str) -> Option<&'a PyPiFile> {
    let candidates: &[&[&str]] = match (os, arch) {
        ("linux", "x86_64") => &[&["manylinux", "x86_64"], &["musllinux", "x86_64"]],
        ("linux", "aarch64") => &[&["manylinux", "aarch64"], &["musllinux", "aarch64"]],
        ("macos", "aarch64") => &[&["macosx", "arm64"], &["macosx", "universal2"]],
        ("macos", "x86_64") => &[&["macosx", "x86_64"], &["macosx", "universal2"]],
        ("windows", "x86_64") => &[&["win_amd64"]],
        ("windows", "aarch64") => &[&["win_arm64"]],
        _ => &[],
    };
    candidates.iter().find_map(|tags| {
        files.iter().find(|file| {
            file.packagetype == "bdist_wheel"
                && !file.yanked
                && file.digests.sha256.is_some()
                && tags.iter().all(|tag| file.filename.contains(tag))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(filename: &str) -> PyPiFile {
        PyPiFile {
            filename: filename.to_string(),
            url: format!("https://files.pythonhosted.org/packages/{filename}"),
            packagetype: "bdist_wheel".to_string(),
            yanked: false,
            digests: PyPiDigests {
                sha256: Some("0".repeat(64)),
            },
        }
    }

    #[test]
    fn wheels_are_picked_per_platform() {
        let files = vec![
            wheel("clang_format-20.1.7-py2.py3-none-macosx_11_0_arm64.whl"),
            wheel("clang_format-20.1.7-py2.py3-none-manylinux_2_17_x86_64.whl"),
            wheel("clang_format-20.1.7-py2.py3-none-musllinux_1_2_x86_64.whl"),
            wheel("clang_format-20.1.7-py2.py3-none-win_amd64.whl"),
        ];

        let linux = select_wheel(&files, "linux", "x86_64").expect("a linux wheel");
        assert!(
            linux.filename.contains("manylinux"),
            "manylinux comes first"
        );
        assert_eq!(
            select_wheel(&files, "windows", "x86_64").map(|file| file.filename.as_str()),
            Some("clang_format-20.1.7-py2.py3-none-win_amd64.whl")
        );
        assert_eq!(
            select_wheel(&files, "macos", "aarch64").map(|file| file.filename.as_str()),
            Some("clang_format-20.1.7-py2.py3-none-macosx_11_0_arm64.whl")
        );
        assert!(select_wheel(&files, "linux", "riscv64").is_none());
    }

    #[test]
    fn wheels_without_a_digest_or_yanked_are_not_used() {
        let mut yanked = wheel("clang_format-20.1.7-py2.py3-none-manylinux_2_17_x86_64.whl");
        yanked.yanked = true;
        let mut undigested = wheel("clang_format-20.1.7-py2.py3-none-win_amd64.whl");
        undigested.digests.sha256 = None;
        let files = vec![yanked, undigested];

        assert!(select_wheel(&files, "linux", "x86_64").is_none());
        assert!(select_wheel(&files, "windows", "x86_64").is_none());
    }

    #[test]
    fn the_catalog_is_addressable_by_id_and_language() {
        for tool in catalog() {
            assert_eq!(tool_by_id(tool.id).map(|found| found.id), Some(tool.id));
        }
        let c_tools = tools_for_language("C")
            .map(|tool| tool.id)
            .collect::<Vec<_>>();
        assert_eq!(c_tools, vec!["clangd", "clang-format"]);
        assert_eq!(tools_for_language("Zig").count(), 0);
    }

    #[test]
    fn a_tool_on_the_given_path_counts_as_present() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let command = super::executable_file_name("clang-format");
        let installed = directory.path().join(&command);
        std::fs::write(&installed, b"").expect("writing the stand-in binary");
        if cfg!(unix) {
            smol::block_on(util::fs::make_file_executable(&installed))
                .expect("marking it executable");
        }

        let states = smol::block_on(check_tools(
            "C",
            Some(directory.path().to_string_lossy().into_owned()),
            directory.path().to_path_buf(),
        ));

        let clang_format = states
            .iter()
            .find(|state| state.id == "clang-format")
            .expect("clang-format is in the catalog for C");
        assert!(clang_format.present);
        assert_eq!(clang_format.path.as_deref(), installed.to_str());
        // Every catalogued tool for the language is reported, so the caller can
        // say which ones are missing rather than "C is not set up".
        assert!(states.iter().any(|state| state.id == "clangd"));
    }

    #[test]
    fn only_bare_command_names_resolve_to_a_managed_tool() {
        assert!(resolve_managed_tool("/usr/bin/clang-format").is_none());
        assert!(resolve_managed_tool("./clang-format").is_none());
    }
}
