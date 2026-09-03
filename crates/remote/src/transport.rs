use std::io::Write;
use std::path::PathBuf;

use crate::{
    RemoteArch, RemoteOs, RemotePlatform,
    json_log::LogRecord,
    protocol::{MESSAGE_LEN_SIZE, message_len_from_buffer, read_message_with_len, write_message},
};
use anyhow::{Context as _, Result};
use futures::{
    AsyncReadExt as _, FutureExt as _, StreamExt as _,
    channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
};
use gpui::{AppContext as _, AsyncApp, Task};
use rpc::proto::Envelope;
use util::command::Child;

pub mod docker;
#[cfg(any(test, feature = "test-support"))]
pub mod mock;
pub mod ssh;
pub mod wsl;

/// Parses the output of `uname -sm` to determine the remote platform.
/// Takes the last line to skip possible shell initialization output.
fn parse_platform(output: &str) -> Result<RemotePlatform> {
    let output = output.trim();
    let uname = output.rsplit_once('\n').map_or(output, |(_, last)| last);
    let Some((os, arch)) = uname.split_once(" ") else {
        anyhow::bail!("unknown uname: {uname:?}")
    };

    let os = match os {
        "Darwin" => RemoteOs::MacOs,
        "Linux" => RemoteOs::Linux,
        _ => anyhow::bail!(
            "Prebuilt remote servers are not yet available for {os:?}. See https://zed.dev/docs/remote-development"
        ),
    };

    // exclude armv5,6,7 as they are 32-bit.
    let arch = if arch.starts_with("armv8")
        || arch.starts_with("armv9")
        || arch.starts_with("arm64")
        || arch.starts_with("aarch64")
    {
        RemoteArch::Aarch64
    } else if arch.starts_with("x86") {
        RemoteArch::X86_64
    } else {
        anyhow::bail!(
            "Prebuilt remote servers are not yet available for {arch:?}. See https://zed.dev/docs/remote-development"
        )
    };

    Ok(RemotePlatform { os, arch })
}

/// The command (program + args) used to read a remote host's OS version, given
/// its detected OS.
///
/// The output is parsed by [`parse_os_version`].
pub(crate) fn os_version_command(os: RemoteOs) -> (&'static str, &'static [&'static str]) {
    match os {
        // Matches the `/etc/os-release` parsing in `client::telemetry::os_version`.
        RemoteOs::Linux => ("cat", &["/etc/os-release"]),
        RemoteOs::MacOs => ("sw_vers", &["-productVersion"]),
        // Prints e.g. "Microsoft Windows [Version 10.0.19045.5011]".
        RemoteOs::Windows => ("cmd.exe", &["/c", "ver"]),
    }
}

/// Parses the output of [`os_version_command`] into a human-readable version
/// string, matching the conventions used by `client::telemetry::os_version`.
///
/// For Linux this is `"{ID} {VERSION_ID}"` (e.g. `"ubuntu 24.04"`); for macOS it
/// is the product version (e.g. `"15.6.1"`); for Windows it is the
/// `major.minor.build` version (e.g. `"10.0.19045"`). Returns `None` if nothing
/// usable could be parsed.
pub(crate) fn parse_os_version(os: RemoteOs, output: &str) -> Option<String> {
    let output = output.trim();
    if output.is_empty() {
        return None;
    }
    match os {
        RemoteOs::Linux => util::parse_os_release(output),
        RemoteOs::MacOs => {
            // `sw_vers -productVersion` prints a single version line.
            output
                .lines()
                .next_back()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
        }
        RemoteOs::Windows => parse_windows_version(output),
    }
}

/// Extracts a `major.minor.build` version from the output of `cmd.exe /c ver`,
/// e.g. `"Microsoft Windows [Version 10.0.19045.5011]"` -> `"10.0.19045"`.
///
/// Scans for the first dotted run of integers (rather than relying on the
/// surrounding, potentially localized, text) and drops the trailing revision so
/// the format matches `client::telemetry::os_version` on Windows.
fn parse_windows_version(output: &str) -> Option<String> {
    output
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(|token| {
            let parts: Vec<&str> = token.split('.').filter(|part| !part.is_empty()).collect();
            (parts.len() >= 3 && parts.iter().all(|part| part.parse::<u32>().is_ok()))
                .then(|| parts[..3].join("."))
        })
        .next()
}

/// Parses the output of `echo $SHELL` to determine the remote shell.
/// Takes the last line to skip possible shell initialization output.
fn parse_shell(output: &str, fallback_shell: &str) -> String {
    let output = output.trim();
    let shell = output.rsplit_once('\n').map_or(output, |(_, last)| last);
    if shell.is_empty() {
        log::error!("$SHELL is not set, falling back to {fallback_shell}");
        fallback_shell.to_owned()
    } else {
        shell.to_owned()
    }
}

fn handle_rpc_messages_over_child_process_stdio(
    mut remote_proxy_process: Child,
    incoming_tx: UnboundedSender<Envelope>,
    mut outgoing_rx: UnboundedReceiver<Envelope>,
    mut connection_activity_tx: Sender<()>,
    cx: &AsyncApp,
) -> Task<Result<i32>> {
    let mut child_stderr = remote_proxy_process.stderr.take().unwrap();
    let mut child_stdout = remote_proxy_process.stdout.take().unwrap();
    let mut child_stdin = remote_proxy_process.stdin.take().unwrap();

    let mut stdin_buffer = Vec::new();
    let mut stdout_buffer = Vec::new();
    let mut stderr_buffer = Vec::new();
    let mut stderr_offset = 0;

    let stdin_task = cx.background_spawn(async move {
        while let Some(outgoing) = outgoing_rx.next().await {
            write_message(&mut child_stdin, &mut stdin_buffer, outgoing).await?;
        }
        anyhow::Ok(())
    });

    let stdout_task = cx.background_spawn({
        let mut connection_activity_tx = connection_activity_tx.clone();
        async move {
            loop {
                stdout_buffer.resize(MESSAGE_LEN_SIZE, 0);
                let len = child_stdout.read(&mut stdout_buffer).await?;

                if len == 0 {
                    return anyhow::Ok(());
                }

                if len < MESSAGE_LEN_SIZE {
                    child_stdout.read_exact(&mut stdout_buffer[len..]).await?;
                }

                let message_len = message_len_from_buffer(&stdout_buffer);
                let envelope =
                    read_message_with_len(&mut child_stdout, &mut stdout_buffer, message_len)
                        .await?;
                connection_activity_tx.try_send(()).ok();
                incoming_tx.unbounded_send(envelope).ok();
            }
        }
    });

    let stderr_task: Task<anyhow::Result<()>> = cx.background_spawn(async move {
        loop {
            stderr_buffer.resize(stderr_offset + 1024, 0);

            let len = child_stderr
                .read(&mut stderr_buffer[stderr_offset..])
                .await?;
            if len == 0 {
                return anyhow::Ok(());
            }

            stderr_offset += len;
            let mut start_ix = 0;
            while let Some(ix) = stderr_buffer[start_ix..stderr_offset]
                .iter()
                .position(|b| b == &b'\n')
            {
                let line_ix = start_ix + ix;
                let content = &stderr_buffer[start_ix..line_ix];
                start_ix = line_ix + 1;
                if let Ok(record) = serde_json::from_slice::<LogRecord>(content) {
                    record.log(log::logger())
                } else {
                    std::io::stderr()
                        .write_fmt(format_args!(
                            "(remote) {}\n",
                            String::from_utf8_lossy(content)
                        ))
                        .ok();
                }
            }
            stderr_buffer.drain(0..start_ix);
            stderr_offset -= start_ix;

            connection_activity_tx.try_send(()).ok();
        }
    });

    cx.background_spawn(async move {
        let result = futures::select! {
            result = stdin_task.fuse() => {
                result.context("stdin")
            }
            result = stdout_task.fuse() => {
                result.context("stdout")
            }
            result = stderr_task.fuse() => {
                result.context("stderr")
            }
        };
        let exit_status = remote_proxy_process.status().await?;
        let status = exit_status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            let status = std::os::unix::process::ExitStatusExt::signal(&exit_status).unwrap_or(1);
            #[cfg(not(unix))]
            let status = 1;
            status
        });
        match result {
            Ok(_) => Ok(status),
            Err(error) => Err(error),
        }
    })
}

/// File names of a remote server artifact for `platform`, most preferred first.
///
/// The compressed name is exactly what `script/bundle-linux` and
/// `script/bundle-mac` emit, so a release artifact can be dropped in as-is. The
/// uncompressed name is accepted too, so a locally cross-compiled binary can be
/// used without a compression step.
pub(crate) fn bundled_server_artifact_names(platform: RemotePlatform) -> [String; 2] {
    let stem = format!("zed-remote-server-{}-{}", platform.os, platform.arch);
    if platform.os.is_windows() {
        [format!("{stem}.zip"), format!("{stem}.exe")]
    } else {
        [format!("{stem}.gz"), stem]
    }
}

fn find_bundled_server_artifact_in_dirs(
    platform: RemotePlatform,
    dirs: &[PathBuf],
) -> Option<PathBuf> {
    let names = bundled_server_artifact_names(platform);
    dirs.iter().find_map(|dir| {
        names
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Directories searched for a remote server artifact shipped with the
/// application, most preferred first: next to the running executable, then
/// its `bin` subdirectory (the same convention as
/// `util::command::resolve_command_next_to_current_exe`).
///
/// Debug builds also search the primary workspace's `target/debug`
/// unconditionally, because `script/build-dev.ps1` always places its
/// bundled artifact there — even when this executable itself was built to a
/// different `--target-dir` (e.g. `target/alt`, used to run a second `dev`
/// instance side by side for testing while `target/debug` holds the main
/// build). Not compiled into release builds: that alternate-target-dir
/// workflow is a dev convenience, and a release binary has no business
/// looking outside its own install directory for a server binary to upload.
///
/// Returns `None` only if the running executable's own path could not be
/// determined; used both to find the artifact and, when nothing was found,
/// to explain in an error message where this looked.
pub(crate) fn bundled_server_search_dirs() -> Option<Vec<PathBuf>> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let bin_dir = exe_dir.join("bin");
    #[allow(unused_mut)]
    let mut dirs = vec![exe_dir, bin_dir];
    #[cfg(debug_assertions)]
    dirs.push(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/debug"
    )));
    Some(dirs)
}

/// Locates a remote server artifact shipped with the application, searching
/// [`bundled_server_search_dirs`].
///
/// This is what lets SSH remoting work for someone who installed nothing extra:
/// the server binary travels with the app, so it can be uploaded without a local
/// cross-compilation toolchain and without reaching the network, which this fork
/// blocks by default.
pub(crate) fn find_bundled_server_artifact(platform: RemotePlatform) -> Option<PathBuf> {
    let dirs = bundled_server_search_dirs()?;
    find_bundled_server_artifact_in_dirs(platform, &dirs)
}

/// Locates the bundled `slirp4netns` binary shipped with the application for
/// ADR-F18's remote Linux sandbox. Mirrors [`find_bundled_server_artifact`]: it
/// looks next to the running executable and in its `bin` subdirectory. Only
/// meaningful for a Linux remote (slirp4netns is Linux-only); returns `None`
/// otherwise. `script/build-dev.ps1` places `slirp4netns-linux-x86_64` next to
/// `zed.exe`.
pub(crate) fn find_bundled_slirp4netns(platform: RemotePlatform) -> Option<PathBuf> {
    if platform.os != RemoteOs::Linux {
        return None;
    }
    let name = format!("slirp4netns-{}-{}", platform.os, platform.arch);
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let bin_dir = exe_dir.join("bin");
    [exe_dir, bin_dir]
        .iter()
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
async fn build_remote_server_from_source(
    platform: &crate::RemotePlatform,
    delegate: &dyn crate::RemoteClientDelegate,
    binary_exists_on_server: bool,
    cx: &mut AsyncApp,
) -> Result<Option<std::path::PathBuf>> {
    use std::env::VarError;
    use std::path::Path;
    use util::command::{Command, Stdio, new_command};

    if let Ok(path) = std::env::var("ZED_COPY_REMOTE_SERVER") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Ok(Some(path));
        } else {
            log::warn!(
                "ZED_COPY_REMOTE_SERVER path does not exist, falling back to ZED_BUILD_REMOTE_SERVER: {}",
                path.display()
            );
        }
    }

    // By default, we make building remote server from source opt-out and we do not force artifact compression
    // for quicker builds.
    let build_remote_server =
        std::env::var("ZED_BUILD_REMOTE_SERVER").unwrap_or("nocompress".into());

    if let "never" = &*build_remote_server {
        return Ok(None);
    } else if let "false" | "no" | "off" | "0" = &*build_remote_server {
        if binary_exists_on_server {
            return Ok(None);
        }
        log::warn!("ZED_BUILD_REMOTE_SERVER is disabled, but no server binary exists on the server")
    }

    async fn run_cmd(command: &mut Command) -> Result<()> {
        let output = command
            .kill_on_drop(true)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "Failed to run command: {command:?}: output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    // `remote_server` pulls in `aws-lc-sys`, whose build script shells out to
    // `cmake` regardless of whether this is a same-arch `cargo build` or a
    // cross-compiled `cargo zigbuild`. A dev environment launched outside a
    // Visual Studio developer shell (e.g. from Explorer or a task runner)
    // will not have `cmake` on `$PATH`, and the resulting build-script
    // failure is easy to mistake for a `zig`/`cargo-zigbuild` problem, so
    // this is checked up front with a Japanese-language explanation instead.
    let cargo_path_env = ensure_cmake_available(cx).await?;

    let use_musl = !build_remote_server.contains("nomusl");
    let triple = format!(
        "{}-{}",
        platform.arch,
        match platform.os {
            RemoteOs::Linux =>
                if use_musl {
                    "unknown-linux-musl"
                } else {
                    "unknown-linux-gnu"
                },
            RemoteOs::MacOs => "apple-darwin",
            RemoteOs::Windows if cfg!(windows) => "pc-windows-msvc",
            RemoteOs::Windows => "pc-windows-gnu",
        }
    );
    let mut rust_flags = match std::env::var("RUSTFLAGS") {
        Ok(val) => val,
        Err(VarError::NotPresent) => String::new(),
        Err(e) => {
            log::error!("Failed to get env var `RUSTFLAGS` value: {e}");
            String::new()
        }
    };
    if platform.os == RemoteOs::Linux && use_musl {
        rust_flags.push_str(" -C target-feature=+crt-static");

        if let Ok(path) = std::env::var("ZED_ZSTD_MUSL_LIB") {
            rust_flags.push_str(&format!(" -C link-arg=-L{path}"));
        }
    }
    if platform.arch.as_str() == std::env::consts::ARCH
        && platform.os.as_str() == std::env::consts::OS
    {
        delegate.set_status(Some("リモートサーバーをソースからビルドしています"), cx);
        log::info!("building remote server binary from source");
        let mut command = new_command("cargo");
        command
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .args([
                "build",
                "--package",
                "remote_server",
                "--features",
                "debug-embed",
                "--target-dir",
                "target/remote_server",
                "--target",
                &triple,
            ])
            .env("RUSTFLAGS", &rust_flags);
        if let Some(cargo_path_env) = &cargo_path_env {
            command.env("PATH", cargo_path_env);
        }
        run_cmd(&mut command).await?;
    } else {
        if which("zig", cx).await?.is_none() {
            anyhow::bail!(if cfg!(not(windows)) {
                "zig not found on $PATH, install zig (see https://ziglang.org/learn/getting-started or use zigup)"
            } else {
                "zig not found on $PATH, install zig (use `winget install -e --id zig.zig` or see https://ziglang.org/learn/getting-started or use zigup)"
            });
        }

        let rustup = which("rustup", cx)
            .await?
            .context("rustup not found on $PATH, install rustup (see https://rustup.rs/)")?;
        delegate.set_status(
            Some("クロスコンパイル用の rustup ターゲットを追加しています"),
            cx,
        );
        log::info!("adding rustup target");
        run_cmd(new_command(rustup).args(["target", "add"]).arg(&triple)).await?;

        if which("cargo-zigbuild", cx).await?.is_none() {
            delegate.set_status(
                Some("クロスコンパイル用の cargo-zigbuild を導入しています"),
                cx,
            );
            log::info!("installing cargo-zigbuild");
            run_cmd(new_command("cargo").args(["install", "--locked", "cargo-zigbuild"])).await?;
        }

        delegate.set_status(
            Some(&format!(
                "{triple} 向けのリモートサーバーを Zig でビルドしています"
            )),
            cx,
        );
        log::info!("building remote binary from source for {triple} with Zig");
        let mut command = new_command("cargo");
        command
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .args([
                "zigbuild",
                "--package",
                "remote_server",
                "--features",
                "debug-embed",
                "--target-dir",
                "target/remote_server",
                "--target",
                &triple,
            ])
            .env("RUSTFLAGS", &rust_flags);
        if let Some(cargo_path_env) = &cargo_path_env {
            command.env("PATH", cargo_path_env);
        }
        run_cmd(&mut command).await?;
    };
    let bin_path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .join("target")
        .join("remote_server")
        .join(&triple)
        .join("debug")
        .join("remote_server")
        .with_extension(if platform.os.is_windows() { "exe" } else { "" });

    let path = if !build_remote_server.contains("nocompress") {
        delegate.set_status(Some("バイナリを圧縮しています"), cx);

        #[cfg(not(target_os = "windows"))]
        let archive_path = {
            run_cmd(new_command("gzip").arg("-f").arg(&bin_path)).await?;
            bin_path.with_extension("gz")
        };

        #[cfg(target_os = "windows")]
        let archive_path = {
            let zip_path = bin_path.with_extension("zip");
            if smol::fs::metadata(&zip_path).await.is_ok() {
                smol::fs::remove_file(&zip_path).await?;
            }
            let compress_command = format!(
                "Compress-Archive -Path '{}' -DestinationPath '{}' -Force",
                bin_path.display(),
                zip_path.display(),
            );
            run_cmd(new_command("powershell.exe").args([
                "-NoProfile",
                "-Command",
                &compress_command,
            ]))
            .await?;
            zip_path
        };

        std::env::current_dir()?.join(archive_path)
    } else {
        bin_path
    };

    Ok(Some(path))
}

/// Streams `path` through SHA-256 and returns the digest as lowercase hex.
///
/// Streamed rather than read whole because the caller hashes a ~90MB server
/// binary, and this runs while a connection is being established.
///
/// Used to answer "does the remote already have exactly this binary?" without
/// uploading it: see `SshRemoteConnection::locally_built_binary_is_already_installed`.
#[cfg(any(test, debug_assertions, feature = "build-remote-server-binary"))]
pub(crate) async fn sha256_of_file(path: &std::path::Path) -> Result<String> {
    use sha2::Digest as _;

    let mut file = smol::fs::File::open(path)
        .await
        .with_context(|| format!("opening {} to hash it", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading {} to hash it", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Reads the digest out of the output of `sha256sum <path>` (coreutils, so
/// Linux) or `shasum -a 256 <path>` (what macOS ships instead). Both print
/// `<64 hex chars><two spaces><path>`.
///
/// Lines are scanned from the back, for the same reason [`parse_platform`] takes
/// the last line: a login shell's initialization can print to stdout ahead of
/// the command's own output.
#[cfg(any(test, debug_assertions, feature = "build-remote-server-binary"))]
pub(crate) fn parse_sha256sum_output(output: &str) -> Option<String> {
    output.lines().rev().find_map(|line| {
        let token = line.split_whitespace().next()?;
        is_sha256_hex(token).then(|| token.to_ascii_lowercase())
    })
}

/// Reads the digest out of the output of `certutil -hashfile <path> SHA256`,
/// which is the only hashing tool a stock Windows remote is guaranteed to have.
/// Its output is three lines:
///
/// ```text
/// SHA256 hash of C:\Users\me\.zed_server\zed-remote-server-dev-build.exe:
/// 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
/// CertUtil: -hashfile command completed successfully.
/// ```
///
/// Whitespace is stripped before the shape is checked because older Windows
/// builds print the digest as space-separated byte pairs.
#[cfg(any(test, debug_assertions, feature = "build-remote-server-binary"))]
pub(crate) fn parse_certutil_hash_output(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let digest: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        is_sha256_hex(&digest).then(|| digest.to_ascii_lowercase())
    })
}

#[cfg(any(test, debug_assertions, feature = "build-remote-server-binary"))]
fn is_sha256_hex(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
async fn which(
    binary_name: impl AsRef<str>,
    cx: &mut AsyncApp,
) -> Result<Option<std::path::PathBuf>> {
    let binary_name = binary_name.as_ref().to_string();
    let binary_name_cloned = binary_name.clone();
    let res = cx
        .background_spawn(async move { which::which(binary_name_cloned) })
        .await;
    match res {
        Ok(path) => Ok(Some(path)),
        Err(which::Error::CannotFindBinaryPath) => Ok(None),
        Err(err) => Err(anyhow::anyhow!(
            "Failed to run 'which' to find the binary '{binary_name}': {err}"
        )),
    }
}

/// Ensures `cmake` is reachable by the `cargo build`/`cargo zigbuild`
/// subprocess that is about to compile `remote_server`: its `aws-lc-sys`
/// dependency shells out to `cmake` from its build script, regardless of
/// whether the target is the host architecture or a cross-compile.
///
/// Returns the `PATH` value to set on that one subprocess, `None` when
/// `cmake` is already on `$PATH` and nothing needs to change. This process's
/// own `$PATH` (and any other process's) is never modified — only the
/// `Command` the caller passes that value to is affected.
#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
async fn ensure_cmake_available(cx: &mut AsyncApp) -> Result<Option<std::ffi::OsString>> {
    if which("cmake", cx).await?.is_some() {
        return Ok(None);
    }

    #[cfg(windows)]
    {
        let program_files = std::env::var("ProgramFiles").unwrap_or_default();
        let program_files_x86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
        if let Some(cmake_dir) = pick_vs_cmake_dir(&program_files, &program_files_x86, |path| {
            path.is_file()
        }) {
            log::info!(
                "cmake not found on $PATH, using the copy bundled with Visual Studio: {}",
                cmake_dir.display()
            );
            let existing_path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![cmake_dir];
            paths.extend(std::env::split_paths(&existing_path));
            let joined = std::env::join_paths(paths)
                .context("cmake のディレクトリを含む PATH を組み立てられませんでした")?;
            return Ok(Some(joined));
        }
        anyhow::bail!(
            "cmake が PATH にありません (remote_server のビルドに必要です)。Visual Studio 同梱の \
             CMake (…\\Common7\\IDE\\CommonExtensions\\Microsoft\\CMake\\CMake\\bin) を PATH に \
             追加するか、script\\build-dev.ps1 でビルドして同梱アーティファクトを用意してください"
        );
    }
    #[cfg(not(windows))]
    anyhow::bail!(
        "cmake が PATH にありません (remote_server のビルドに必要です)。cmake をインストールしてください"
    );
}

/// Directories where Visual Studio 2022 ships its own copy of CMake, most
/// preferred first, searched when `cmake` is not already on `$PATH`. Mirrors
/// `script/build-dev.ps1`'s own CMake search (see BUILD-WINDOWS.md) so a
/// machine that can run that script can also drive this in-process
/// cross-compile path without the developer having to launch Zed from a
/// Visual Studio developer shell.
///
/// A pure function of its two roots so it can be unit tested without relying
/// on `%ProgramFiles%`/`%ProgramFiles(x86)%` actually being set the same way
/// in the test process.
#[cfg(all(windows, any(debug_assertions, feature = "build-remote-server-binary")))]
fn vs_cmake_candidate_dirs(program_files: &str, program_files_x86: &str) -> Vec<PathBuf> {
    const EDITIONS: [&str; 4] = ["Enterprise", "Professional", "Community", "BuildTools"];
    [program_files, program_files_x86]
        .into_iter()
        .filter(|root| !root.is_empty())
        .flat_map(|root| {
            EDITIONS.iter().map(move |edition| {
                std::path::Path::new(root)
                    .join("Microsoft Visual Studio")
                    .join("2022")
                    .join(edition)
                    .join("Common7")
                    .join("IDE")
                    .join("CommonExtensions")
                    .join("Microsoft")
                    .join("CMake")
                    .join("CMake")
                    .join("bin")
            })
        })
        .collect()
}

/// Picks the first [`vs_cmake_candidate_dirs`] entry that holds `cmake.exe`,
/// judged by the injected `cmake_exe_exists` rather than the real filesystem.
/// Keeping the existence check as a parameter is what lets this whole
/// selection be unit tested without depending on the environment cmake is
/// actually missing from in the failure case this exists to handle.
#[cfg(all(windows, any(debug_assertions, feature = "build-remote-server-binary")))]
fn pick_vs_cmake_dir(
    program_files: &str,
    program_files_x86: &str,
    cmake_exe_exists: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    vs_cmake_candidate_dirs(program_files, program_files_x86)
        .into_iter()
        .find(|dir| cmake_exe_exists(&dir.join("cmake.exe")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_x86_64() -> RemotePlatform {
        RemotePlatform {
            os: RemoteOs::Linux,
            arch: RemoteArch::X86_64,
        }
    }

    #[cfg(all(windows, any(debug_assertions, feature = "build-remote-server-binary")))]
    #[test]
    fn test_vs_cmake_candidate_dirs() {
        let dirs = vs_cmake_candidate_dirs(r"C:\Program Files", r"C:\Program Files (x86)");
        assert_eq!(dirs.len(), 8);
        assert!(dirs.contains(&PathBuf::from(
            r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
        )));
        assert!(dirs.contains(&PathBuf::from(
            r"C:\Program Files\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
        )));
        assert!(dirs.contains(&PathBuf::from(
            r"C:\Program Files (x86)\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
        )));

        // Empty roots (an unset env var) contribute no candidates instead of
        // producing paths rooted at "".
        assert_eq!(vs_cmake_candidate_dirs("", "").len(), 0);
        assert_eq!(vs_cmake_candidate_dirs(r"C:\Program Files", "").len(), 4);
    }

    #[cfg(all(windows, any(debug_assertions, feature = "build-remote-server-binary")))]
    #[test]
    fn test_pick_vs_cmake_dir() {
        // No VS installed at all (or none of the candidate editions match):
        // nothing found. The injected existence check means this never
        // touches the real filesystem, so the test works the same whether
        // or not cmake is actually on this machine's $PATH.
        assert_eq!(pick_vs_cmake_dir(r"C:\Program Files", "", |_| false), None);

        // Only the Community edition's cmake.exe "exists": that one is
        // picked, and its parent directory is what gets returned (the `bin`
        // directory to add to $PATH, not the exe itself).
        let community_bin = PathBuf::from(
            r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin",
        );
        let community_exe = community_bin.join("cmake.exe");
        assert_eq!(
            pick_vs_cmake_dir(r"C:\Program Files", "", |path| path == community_exe),
            Some(community_bin)
        );

        // Neither root set: nothing to pick, regardless of the existence
        // check.
        assert_eq!(pick_vs_cmake_dir("", "", |_| true), None);
    }

    #[test]
    fn test_bundled_server_artifact_names() {
        assert_eq!(
            bundled_server_artifact_names(linux_x86_64()),
            [
                "zed-remote-server-linux-x86_64.gz".to_string(),
                "zed-remote-server-linux-x86_64".to_string()
            ]
        );
        assert_eq!(
            bundled_server_artifact_names(RemotePlatform {
                os: RemoteOs::MacOs,
                arch: RemoteArch::Aarch64,
            }),
            [
                "zed-remote-server-macos-aarch64.gz".to_string(),
                "zed-remote-server-macos-aarch64".to_string()
            ]
        );
        assert_eq!(
            bundled_server_artifact_names(RemotePlatform {
                os: RemoteOs::Windows,
                arch: RemoteArch::X86_64,
            }),
            [
                "zed-remote-server-windows-x86_64.zip".to_string(),
                "zed-remote-server-windows-x86_64.exe".to_string()
            ]
        );
    }

    #[test]
    fn test_find_bundled_server_artifact_prefers_compressed_next_to_exe() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let dirs = [dir.path().to_path_buf(), bin_dir];

        assert_eq!(
            find_bundled_server_artifact_in_dirs(linux_x86_64(), &dirs),
            None
        );

        let uncompressed = dir.path().join("zed-remote-server-linux-x86_64");
        std::fs::write(&uncompressed, b"").unwrap();
        assert_eq!(
            find_bundled_server_artifact_in_dirs(linux_x86_64(), &dirs).as_deref(),
            Some(uncompressed.as_path())
        );

        let compressed = dir.path().join("zed-remote-server-linux-x86_64.gz");
        std::fs::write(&compressed, b"").unwrap();
        assert_eq!(
            find_bundled_server_artifact_in_dirs(linux_x86_64(), &dirs).as_deref(),
            Some(compressed.as_path())
        );
    }

    #[test]
    fn test_find_bundled_server_artifact_falls_back_to_bin_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let bundled = bin_dir.join("zed-remote-server-linux-x86_64.gz");
        std::fs::write(&bundled, b"").unwrap();

        assert_eq!(
            find_bundled_server_artifact_in_dirs(
                linux_x86_64(),
                &[dir.path().to_path_buf(), bin_dir]
            )
            .as_deref(),
            Some(bundled.as_path())
        );
    }

    #[test]
    fn test_find_bundled_server_artifact_ignores_other_platforms() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zed-remote-server-macos-aarch64.gz"), b"").unwrap();
        std::fs::write(dir.path().join("zed-remote-server-linux-aarch64.gz"), b"").unwrap();

        assert_eq!(
            find_bundled_server_artifact_in_dirs(linux_x86_64(), &[dir.path().to_path_buf()]),
            None
        );
    }

    #[test]
    fn test_find_bundled_server_artifact_ignores_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zed-remote-server-linux-x86_64.gz")).unwrap();

        assert_eq!(
            find_bundled_server_artifact_in_dirs(linux_x86_64(), &[dir.path().to_path_buf()]),
            None
        );
    }

    #[test]
    fn test_parse_platform() {
        let result = parse_platform("Linux x86_64\n").unwrap();
        assert_eq!(result.os, RemoteOs::Linux);
        assert_eq!(result.arch, RemoteArch::X86_64);

        let result = parse_platform("Darwin arm64\n").unwrap();
        assert_eq!(result.os, RemoteOs::MacOs);
        assert_eq!(result.arch, RemoteArch::Aarch64);

        let result = parse_platform("Linux x86_64").unwrap();
        assert_eq!(result.os, RemoteOs::Linux);
        assert_eq!(result.arch, RemoteArch::X86_64);

        let result = parse_platform("some shell init output\nLinux aarch64\n").unwrap();
        assert_eq!(result.os, RemoteOs::Linux);
        assert_eq!(result.arch, RemoteArch::Aarch64);

        let result = parse_platform("some shell init output\nLinux aarch64").unwrap();
        assert_eq!(result.os, RemoteOs::Linux);
        assert_eq!(result.arch, RemoteArch::Aarch64);

        assert_eq!(
            parse_platform("Linux armv8l\n").unwrap().arch,
            RemoteArch::Aarch64
        );
        assert_eq!(
            parse_platform("Linux aarch64\n").unwrap().arch,
            RemoteArch::Aarch64
        );
        assert_eq!(
            parse_platform("Linux x86_64\n").unwrap().arch,
            RemoteArch::X86_64
        );

        let result = parse_platform(
            r#"Linux x86_64 - What you're referring to as Linux, is in fact, GNU/Linux...\n"#,
        )
        .unwrap();
        assert_eq!(result.os, RemoteOs::Linux);
        assert_eq!(result.arch, RemoteArch::X86_64);

        assert!(parse_platform("Windows x86_64\n").is_err());
        assert!(parse_platform("Linux armv7l\n").is_err());
    }

    #[test]
    fn test_parse_os_version() {
        // Linux delegates to `util::parse_os_release` (tested there); confirm
        // the dispatch is wired up.
        let os_release = "ID=ubuntu\nVERSION_ID=\"24.04\"\n";
        assert_eq!(
            parse_os_version(RemoteOs::Linux, os_release),
            Some("ubuntu 24.04".to_string())
        );

        // macOS `sw_vers -productVersion` prints a bare version, possibly after
        // shell initialization noise.
        assert_eq!(
            parse_os_version(RemoteOs::MacOs, "15.6.1\n"),
            Some("15.6.1".to_string())
        );
        assert_eq!(
            parse_os_version(RemoteOs::MacOs, "shell noise\n26.0\n"),
            Some("26.0".to_string())
        );
        assert_eq!(parse_os_version(RemoteOs::MacOs, ""), None);

        // Windows `cmd.exe /c ver`, with the trailing revision dropped to match
        // the `major.minor.build` format used by local Windows telemetry.
        assert_eq!(
            parse_os_version(
                RemoteOs::Windows,
                "Microsoft Windows [Version 10.0.19045.5011]\n"
            ),
            Some("10.0.19045".to_string())
        );
        // Localized output: only the version number is relied upon.
        assert_eq!(
            parse_os_version(
                RemoteOs::Windows,
                "Microsoft Windows [Versione 10.0.22631.1]"
            ),
            Some("10.0.22631".to_string())
        );
        assert_eq!(parse_os_version(RemoteOs::Windows, "no version here"), None);
    }

    #[test]
    fn test_sha256_of_file() {
        use sha2::Digest as _;

        let dir = tempfile::tempdir().unwrap();

        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            smol::block_on(sha256_of_file(&empty)).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let hello = dir.path().join("hello");
        std::fs::write(&hello, b"hello world").unwrap();
        assert_eq!(
            smol::block_on(sha256_of_file(&hello)).unwrap(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );

        // Larger than the read buffer, so the streaming loop is what is being
        // checked rather than a single read.
        let large_contents: Vec<u8> = (0..400 * 1024).map(|byte| byte as u8).collect();
        let large = dir.path().join("large");
        std::fs::write(&large, &large_contents).unwrap();
        assert_eq!(
            smol::block_on(sha256_of_file(&large)).unwrap(),
            hex::encode(sha2::Sha256::digest(&large_contents))
        );

        assert!(smol::block_on(sha256_of_file(&dir.path().join("absent"))).is_err());
    }

    #[test]
    fn test_parse_sha256sum_output() {
        let digest = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

        // `sha256sum` and `shasum -a 256` both print `<hex>  <path>`.
        assert_eq!(
            parse_sha256sum_output(&format!(
                "{digest}  /home/user/.zed_server/zed-remote-server-dev-build\n"
            ))
            .as_deref(),
            Some(digest)
        );
        // No trailing newline, and a path containing spaces.
        assert_eq!(
            parse_sha256sum_output(&format!("{digest}  /home/us er/zed remote server")).as_deref(),
            Some(digest)
        );
        // Shell initialization output ahead of the command's own, as
        // `parse_platform` and `parse_shell` also have to cope with.
        assert_eq!(
            parse_sha256sum_output(&format!("some shell init output\n{digest}  /path\n"))
                .as_deref(),
            Some(digest)
        );
        assert_eq!(
            parse_sha256sum_output(&format!("{}  /path\n", digest.to_ascii_uppercase())).as_deref(),
            Some(digest)
        );

        assert_eq!(parse_sha256sum_output(""), None);
        assert_eq!(
            parse_sha256sum_output("sha256sum: /path: No such file or directory\n"),
            None
        );
        // Not 64 characters, and not hex.
        assert_eq!(
            parse_sha256sum_output(&format!("{}  /path\n", &digest[1..])),
            None
        );
        assert_eq!(
            parse_sha256sum_output(&format!("z{}  /path\n", &digest[1..])),
            None
        );
    }

    #[test]
    fn test_parse_certutil_hash_output() {
        let digest = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

        assert_eq!(
            parse_certutil_hash_output(&format!(
                "SHA256 hash of C:\\Users\\me\\.zed_server\\zed-remote-server-dev-build.exe:\r\n\
                 {digest}\r\n\
                 CertUtil: -hashfile command completed successfully.\r\n"
            ))
            .as_deref(),
            Some(digest)
        );

        // Older Windows builds print the digest as space-separated byte pairs.
        let spaced = digest
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_certutil_hash_output(&format!(
                "SHA256 hash of C:\\path:\r\n{spaced}\r\nCertUtil: -hashfile command completed successfully.\r\n"
            ))
            .as_deref(),
            Some(digest)
        );

        assert_eq!(
            parse_certutil_hash_output(
                "CertUtil: -hashfile command FAILED: 0x80070002 (WIN32: 2 ERROR_FILE_NOT_FOUND)\r\n"
            ),
            None
        );
        assert_eq!(parse_certutil_hash_output(""), None);
    }

    #[test]
    fn test_parse_shell() {
        assert_eq!(parse_shell("/bin/bash\n", "sh"), "/bin/bash");
        assert_eq!(parse_shell("/bin/zsh\n", "sh"), "/bin/zsh");

        assert_eq!(parse_shell("/bin/bash", "sh"), "/bin/bash");
        assert_eq!(
            parse_shell("some shell init output\n/bin/bash\n", "sh"),
            "/bin/bash"
        );
        assert_eq!(
            parse_shell("some shell init output\n/bin/bash", "sh"),
            "/bin/bash"
        );
        assert_eq!(parse_shell("", "sh"), "sh");
        assert_eq!(parse_shell("\n", "sh"), "sh");
    }
}
