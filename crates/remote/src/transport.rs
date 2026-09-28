use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
use util::rel_path::RelPath;

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

/// The version part of the name the server binary is installed under
/// (`zed-remote-server-{channel}-{this}`).
///
/// A binary that runs under the expected name is reused over SSH, so the name
/// has to change whenever the wire format does: a server of the same Zed
/// version from another revision of this fork, or upstream's own, numbers the
/// fork's messages and fields differently and would drop or misread them.
/// The name only picks the binary; the connection then checks the revision
/// itself (`CheckWireRevision`) and reinstalls a binary that answers another.
/// That holds for dev builds too: when the source build fails or
/// `ZED_BUILD_REMOTE_SERVER=never`, SSH reuses whatever dev binary the host
/// already has.
pub(crate) fn server_binary_version(
    release_channel: release_channel::ReleaseChannel,
    version: impl std::fmt::Display,
) -> String {
    let version = match release_channel {
        release_channel::ReleaseChannel::Dev => "build".to_string(),
        _ => version.to_string(),
    };
    format!(
        "{version}-jp{}",
        rpc::proto::jp_enhance::JP_ENHANCE_WIRE_REVISION
    )
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

/// Where a WSL or Docker connection gets the server binary it installs, in the
/// order the SSH transport tries them: a binary just built from source (dev
/// builds), the copy the remote already holds (so the bundled binary is not
/// uploaded again on every connection), the artifact bundled with the app (so a
/// first connection works offline), and only then a download.
#[derive(Debug, PartialEq)]
pub(crate) enum ServerBinarySource {
    LocallyBuilt(PathBuf),
    AlreadyInstalled,
    Bundled(PathBuf),
    Download,
}

pub(crate) fn choose_server_binary_source(
    locally_built: Option<PathBuf>,
    binary_exists_on_server: bool,
    bundled: Option<PathBuf>,
) -> ServerBinarySource {
    match (locally_built, binary_exists_on_server, bundled) {
        (Some(path), _, _) => ServerBinarySource::LocallyBuilt(path),
        (None, true, _) => ServerBinarySource::AlreadyInstalled,
        (None, false, Some(path)) => ServerBinarySource::Bundled(path),
        (None, false, None) => ServerBinarySource::Download,
    }
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

/// ADR-F18: where the remote sandbox helper looks for `slirp4netns`. The helper
/// resolves it from the directory of its own executable — the remote server
/// binary — so it has to sit beside that, under exactly this name.
pub(crate) fn remote_slirp4netns_path() -> Result<Arc<RelPath>> {
    Ok(paths::remote_server_dir_relative()
        .join(
            RelPath::from_unix_str("slirp4netns")
                .context("building the remote slirp4netns path")?,
        )
        .into())
}

/// A temporary name in the remote server directory to upload `src_path` to
/// before it is moved into place. Unique per upload — process id, clock and a
/// counter — because a process id alone repeats across machines that share a
/// remote, and within one process across installs.
pub(crate) fn remote_upload_temp_path(src_path: &Path) -> Result<Arc<RelPath>> {
    static UPLOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let file_name = src_path
        .file_name()
        .with_context(|| format!("no file name in {}", src_path.display()))?
        .to_string_lossy();
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_nanos());
    let upload = UPLOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(paths::remote_server_dir_relative()
        .join(
            RelPath::from_unix_str(&format!(
                "download-{}-{clock:x}-{upload}-{file_name}",
                std::process::id()
            ))
            .context("building the remote temporary upload path")?,
        )
        .into())
}

/// ADR-F18: a `slirp4netns` that could not be installed only costs sandboxed
/// agent commands their network access (the helper refuses to run the ones
/// that ask for it), so it must not fail a connection that human terminals can
/// still use. Silently degrading meant the user only found out when an agent
/// command refused to run, hours later and in a place that does not explain
/// why; every transport says it while the connection is still the thing they
/// are looking at.
pub(crate) fn warn_slirp4netns_not_installed(error: &anyhow::Error) {
    log::warn!("could not install slirp4netns for the remote sandbox: {error:#}");
    ssh::CONNECTION_WARNINGS.lock().push(format!(
        "リモートのサンドボックス用ヘルパー (slirp4netns) を配置できませんでした。\
         ネットワークを使うエージェントのコマンドはサンドボックスを作れないため実行されません。\
         リモートの `~/.zed_server/` に書き込めるか (空き容量・権限) と、Zed 本体と同じフォルダーに\
         同梱の slirp4netns があるかを確認して再接続してください: {error:#}"
    ));
}

#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
async fn build_remote_server_from_source(
    platform: &crate::RemotePlatform,
    delegate: &dyn crate::RemoteClientDelegate,
    binary_exists_on_server: bool,
    cx: &mut AsyncApp,
) -> Result<Option<std::path::PathBuf>> {
    use std::env::VarError;
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
    // Only the log gets the target triple. Shown in the connection dialog, the
    // triple's `unknown` vendor field ("x86_64-unknown-linux-musl") reads as a
    // defect rather than as Rust's name for "no vendor", and was reported as
    // one; the same facts are spelled out for the user instead.
    let platform_label = {
        let libc = match (platform.os, use_musl) {
            (RemoteOs::Linux, true) => "、musl",
            (RemoteOs::Linux, false) => "、glibc",
            _ => "",
        };
        format!(
            "{} {}{}",
            platform.os.display_name(),
            platform.arch.as_str(),
            libc
        )
    };
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
        delegate.set_status(
            Some(&format!(
                "開発ビルドのため、リモート開発サーバー ({platform_label} 向け) をソースからビルドしています"
            )),
            cx,
        );
        log::info!(
            "building remote server binary from source for {triple} \
             (dev build: release builds upload the bundled archive instead)"
        );
        let mut command = new_command("cargo");
        command
            .current_dir(
                util::dev_repo_root()
                    .context("locating the zed checkout to build remote_server from source")?,
            )
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
        log::info!("adding rustup target {triple}");
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
                "開発ビルドのため、リモート開発サーバー ({platform_label} 向け) を Zig でビルドしています"
            )),
            cx,
        );
        log::info!(
            "building remote binary from source for {triple} with Zig \
             (dev build: release builds upload the bundled archive instead)"
        );
        let mut command = new_command("cargo");
        command
            .current_dir(
                util::dev_repo_root()
                    .context("locating the zed checkout to build remote_server from source")?,
            )
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
    let bin_path = util::dev_repo_root()
        .context("locating the zed checkout that built remote_server from source")?
        .join("target")
        .join("remote_server")
        .join(&triple)
        .join("debug")
        .join("remote_server")
        .with_extension(if platform.os.is_windows() { "exe" } else { "" });

    let path = if !build_remote_server.contains("nocompress") {
        delegate.set_status(Some("リモート開発サーバーを圧縮しています"), cx);

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
/// uploading it: see `SshRemoteConnection::locally_built_binary_is_already_installed`,
/// and the WSL and Docker transports' check of an installed `slirp4netns`.
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

fn is_sha256_hex(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A remembered "the remote already holds exactly this build" verdict, so that
/// the next connection to the same remote does not have to prove it again.
///
/// Proving it means reading the whole server binary twice — once here, once on
/// the remote — and that binary is around 600MB in a dev build. Measured on
/// 2026-09-04 that comparison was 10 of the ~15 seconds a warm connection took.
/// The answer only changes when one of the two copies changes, so both are
/// pinned by size and modification time: a rebuilt local artifact, or a remote
/// binary someone else replaced, invalidates the entry and the digests get
/// computed again. Nothing here is trusted for correctness beyond that — the
/// caller still requires that the remote's copy actually *ran*.
///
/// The WSL and Docker transports keep the same record for a different purpose
/// (ADR-F46): the digest the remote reported right after this app installed the
/// bundled server there, which a later connection has to see again before it
/// reuses that binary as the agent sandbox's helper.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RemoteServerInstall {
    pub local_size: u64,
    pub local_mtime_nanos: u64,
    pub remote_size: u64,
    pub remote_mtime_secs: i64,
    /// Carried so the "skipping upload" log line can still name the build that
    /// both sides agreed on. The verdict itself rests on the four stats above.
    pub sha256: String,
    /// Seconds since the epoch, used only to decide which entries to drop when
    /// the file has grown past [`MAX_REMOTE_SERVER_INSTALLS`].
    pub recorded_at: i64,
}

/// How many `(remote, installed binary name)` pairs the cache keeps. A dev
/// build's binary name carries the commit it was built from, so entries would
/// otherwise accumulate one per revision ever connected with.
const MAX_REMOTE_SERVER_INSTALLS: usize = 32;

fn remote_server_install_cache_path() -> PathBuf {
    paths::data_dir().join("remote_server_installs.json")
}

/// `(size, modification time in nanoseconds since the epoch)` of a local file,
/// or `None` if it cannot be stat'ed or predates the epoch.
pub(crate) async fn local_file_stat(path: &std::path::Path) -> Option<(u64, u64)> {
    let metadata = smol::fs::metadata(path).await.ok()?;
    let modified = metadata.modified().ok()?;
    let nanos = modified
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((metadata.len(), u64::try_from(nanos).ok()?))
}

/// One remote command reporting a file's SHA-256 and, where GNU `stat` exists,
/// its `(size, mtime)`. The path arrives as `$1`, so it needs no quoting, and a
/// missing `stat` does not cost the digest.
pub(crate) const REMOTE_FILE_DIGEST_SCRIPT: &str =
    "sha256sum \"$1\" && { stat -c '%s %Y' \"$1\" || true; }";

/// What the remote reported about one of its files through
/// [`REMOTE_FILE_DIGEST_SCRIPT`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileDigest {
    pub sha256: String,
    pub stat: Option<(u64, i64)>,
}

pub(crate) fn parse_remote_file_digest(output: &str) -> Option<RemoteFileDigest> {
    Some(RemoteFileDigest {
        sha256: parse_sha256sum_output(output)?,
        stat: parse_remote_stat_output(output),
    })
}

/// Reads `(size, mtime in seconds since the epoch)` out of the output of
/// `stat -c "%s %Y" <path>` (GNU coreutils) or `stat -f "%z %m" <path>` (BSD,
/// so macOS). Both print the two numbers separated by a space.
///
/// Lines are scanned from the back for the same reason [`parse_platform`] takes
/// the last line: a login shell's initialization can print ahead of the
/// command's own output.
pub(crate) fn parse_remote_stat_output(output: &str) -> Option<(u64, i64)> {
    output.lines().rev().find_map(|line| {
        let mut fields = line.split_whitespace();
        let size = fields.next()?.parse::<u64>().ok()?;
        let mtime = fields.next()?.parse::<i64>().ok()?;
        fields.next().is_none().then_some((size, mtime))
    })
}

/// The verdict remembered for `key`, if any. Every failure mode (no file yet,
/// unreadable, written by a future version in a shape this cannot parse) is a
/// `None`, which only costs the caller the comparison it was going to do anyway.
pub(crate) async fn read_remote_server_install(key: &str) -> Option<RemoteServerInstall> {
    let contents = smol::fs::read(remote_server_install_cache_path())
        .await
        .ok()?;
    let mut installs: std::collections::HashMap<String, RemoteServerInstall> =
        serde_json::from_slice(&contents).ok()?;
    installs.remove(key)
}

/// Records `install` for `key`, replacing any previous verdict for the same
/// remote and binary. Best effort: this only ever saves work on a later
/// connection, so a failure to write is logged and otherwise ignored.
pub(crate) async fn record_remote_server_install(key: &str, install: RemoteServerInstall) {
    let path = remote_server_install_cache_path();
    let mut installs: std::collections::HashMap<String, RemoteServerInstall> =
        match smol::fs::read(&path).await {
            Ok(contents) => serde_json::from_slice(&contents).unwrap_or_default(),
            Err(_) => Default::default(),
        };
    installs.insert(key.to_owned(), install);

    if installs.len() > MAX_REMOTE_SERVER_INSTALLS {
        let mut recorded_at: Vec<i64> = installs
            .values()
            .map(|install| install.recorded_at)
            .collect();
        recorded_at.sort_unstable();
        if let Some(&oldest_kept) =
            recorded_at.get(installs.len().saturating_sub(MAX_REMOTE_SERVER_INSTALLS))
        {
            installs.retain(|_, install| install.recorded_at >= oldest_kept);
        }
    }

    let write = async {
        let contents = serde_json::to_vec(&installs)?;
        if let Some(parent) = path.parent() {
            smol::fs::create_dir_all(parent).await?;
        }
        smol::fs::write(&path, contents).await?;
        anyhow::Ok(())
    };
    if let Err(error) = write.await {
        log::warn!(
            "could not record which remote server build {} holds: {error:#}",
            path.display()
        );
    }
}

/// Seconds since the epoch, or 0 if the clock is set before it.
pub(crate) fn now_in_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs() as i64)
}

/// ADR-F46: whether the server binary a WSL or Docker connection finds at
/// `install_key` is the one this app installed there from `bundled` — the
/// remote reports the digest it reported right after that install, and
/// `bundled` has not changed since. A binary of unknown origin, such as an
/// upstream build under the same name or one replaced while agent commands
/// still ran unwrapped, answers `version` just as well, and it is the helper
/// every sandboxed agent command starts with.
pub(crate) async fn installed_server_binary_is_verified(
    install_key: &str,
    bundled: &Path,
    installed: Option<&RemoteFileDigest>,
) -> bool {
    let (Some(installed), Some(record), Some((local_size, local_mtime_nanos))) = (
        installed,
        read_remote_server_install(install_key).await,
        local_file_stat(bundled).await,
    ) else {
        return false;
    };
    record.sha256 == installed.sha256
        && record.local_size == local_size
        && record.local_mtime_nanos == local_mtime_nanos
}

/// Remembers what the remote reported right after this app installed the
/// bundled server binary at `install_key`, for
/// [`installed_server_binary_is_verified`] to hold later connections to.
pub(crate) async fn record_verified_server_binary(
    install_key: &str,
    bundled: &Path,
    installed: Option<RemoteFileDigest>,
) {
    let (Some(installed), Some((local_size, local_mtime_nanos))) =
        (installed, local_file_stat(bundled).await)
    else {
        log::warn!(
            "could not pin the server binary just installed at {install_key}; \
             the next connection installs it again"
        );
        return;
    };
    let (remote_size, remote_mtime_secs) = installed.stat.unwrap_or_default();
    record_remote_server_install(
        install_key,
        RemoteServerInstall {
            local_size,
            local_mtime_nanos,
            remote_size,
            remote_mtime_secs,
            sha256: installed.sha256,
            recorded_at: now_in_seconds(),
        },
    )
    .await;
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
        if let Some(cmake_dir) =
            pick_vs_cmake_dir(&program_files, &program_files_x86, |path| path.is_file())
        {
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
    fn test_parse_remote_stat_output() {
        // GNU `stat -c "%s %Y"` and BSD `stat -f "%z %m"` print the same shape.
        assert_eq!(
            parse_remote_stat_output("586446160 1757000890\n"),
            Some((586446160, 1757000890))
        );
        assert_eq!(
            parse_remote_stat_output("some shell init output\n42 7\n"),
            Some((42, 7))
        );
        // Both `stat` spellings failed, so the command printed nothing.
        assert_eq!(parse_remote_stat_output(""), None);
        assert_eq!(parse_remote_stat_output("stat: cannot stat 'x'\n"), None);
        // A third field means this is not the output we think it is.
        assert_eq!(parse_remote_stat_output("42 7 9\n"), None);
        assert_eq!(parse_remote_stat_output("%s %Y\n"), None);
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

    #[test]
    fn test_remote_slirp4netns_path_is_beside_the_server_binary() {
        let slirp4netns = remote_slirp4netns_path().unwrap();
        assert_eq!(
            slirp4netns.display(util::paths::PathStyle::Unix),
            ".zed_server/slirp4netns"
        );
        let server_binary: Arc<RelPath> = paths::remote_server_dir_relative()
            .join(RelPath::from_unix_str("zed-remote-server-stable-1.17.2").unwrap())
            .into();
        assert_eq!(slirp4netns.parent(), server_binary.parent());
    }

    #[test]
    fn test_remote_upload_temp_path() {
        let source = Path::new("bundle").join("slirp4netns-linux-x86_64");
        let first = remote_upload_temp_path(&source).unwrap();
        let second = remote_upload_temp_path(&source).unwrap();
        for temp_path in [&first, &second] {
            let temp_path = temp_path.display(util::paths::PathStyle::Unix);
            assert!(
                temp_path.starts_with(&format!(".zed_server/download-{}-", std::process::id())),
                "{temp_path}"
            );
            assert!(
                temp_path.ends_with("-slirp4netns-linux-x86_64"),
                "{temp_path}"
            );
        }
        assert_ne!(
            first, second,
            "two uploads must never share a temporary name"
        );
        assert!(remote_upload_temp_path(Path::new("..")).is_err());
    }

    #[test]
    fn test_parse_remote_file_digest() {
        let digest = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(
            parse_remote_file_digest(&format!(
                "{digest}  .zed_server/slirp4netns\n2346328 1756800000\n"
            )),
            Some(RemoteFileDigest {
                sha256: digest.to_string(),
                stat: Some((2346328, 1756800000)),
            })
        );
        // Without GNU `stat` the digest alone still counts.
        assert_eq!(
            parse_remote_file_digest(&format!("shell noise\n{digest}  .zed_server/slirp4netns\n")),
            Some(RemoteFileDigest {
                sha256: digest.to_string(),
                stat: None,
            })
        );
        assert_eq!(
            parse_remote_file_digest(
                "sha256sum: .zed_server/slirp4netns: No such file or directory\n"
            ),
            None
        );
    }

    #[test]
    fn test_choose_server_binary_source() {
        let built = PathBuf::from("built");
        let bundled = PathBuf::from("bundled");

        assert_eq!(
            choose_server_binary_source(Some(built.clone()), true, Some(bundled.clone())),
            ServerBinarySource::LocallyBuilt(built)
        );
        // Reusing the installed copy keeps the bundled binary from being
        // uploaded again on every connection.
        assert_eq!(
            choose_server_binary_source(None, true, Some(bundled.clone())),
            ServerBinarySource::AlreadyInstalled
        );
        // A first connection takes the bundled artifact before trying the
        // network, so it works offline.
        assert_eq!(
            choose_server_binary_source(None, false, Some(bundled.clone())),
            ServerBinarySource::Bundled(bundled)
        );
        assert_eq!(
            choose_server_binary_source(None, false, None),
            ServerBinarySource::Download
        );
    }
}
