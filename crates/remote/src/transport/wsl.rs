use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions},
    transport::{parse_platform, parse_shell},
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use collections::HashMap;
use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext as _, AsyncApp, Task};
use release_channel::{AppVersion, ReleaseChannel};
use rpc::proto::Envelope;
use semver::Version;
use smol::{
    fs,
    io::{self, AsyncWriteExt as _},
};
use std::{
    ffi::OsStr,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use util::{
    command::Stdio,
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
    shell::{Shell, ShellKind},
    shell_builder::ShellBuilder,
};

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct WslConnectionOptions {
    pub distro_name: String,
    pub user: Option<String>,
}

impl From<settings::WslConnection> for WslConnectionOptions {
    fn from(val: settings::WslConnection) -> Self {
        WslConnectionOptions {
            distro_name: val.distro_name,
            user: val.user,
        }
    }
}

#[derive(Debug)]
pub(crate) struct WslRemoteConnection {
    remote_binary_path: Option<Arc<RelPath>>,
    platform: RemotePlatform,
    os_version: Option<String>,
    shell: String,
    shell_kind: ShellKind,
    default_system_shell: String,
    has_wsl_interop: bool,
    connection_options: WslConnectionOptions,
}

impl WslRemoteConnection {
    pub(crate) async fn new(
        connection_options: WslConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        log::info!(
            "Connecting to WSL distro {} with user {:?}",
            connection_options.distro_name,
            connection_options.user
        );
        let (release_channel, version) =
            cx.update(|cx| (ReleaseChannel::global(cx), AppVersion::global(cx)));

        let mut this = Self {
            connection_options,
            remote_binary_path: None,
            platform: RemotePlatform {
                os: RemoteOs::Linux,
                arch: RemoteArch::X86_64,
            },
            os_version: None,
            shell: String::new(),
            shell_kind: ShellKind::Posix,
            default_system_shell: String::from("/bin/sh"),
            has_wsl_interop: false,
        };
        delegate.set_status(Some(i18n::t!("Detecting WSL environment")), cx);
        this.shell = this
            .detect_shell()
            .await
            .context("failed detecting shell")?;
        log::info!("Remote shell discovered: {}", this.shell);
        this.shell_kind = ShellKind::new(&this.shell, false);
        this.has_wsl_interop = this.detect_has_wsl_interop().await.unwrap_or_default();
        log::info!(
            "Remote has wsl interop {}",
            if this.has_wsl_interop {
                "enabled"
            } else {
                "disabled"
            }
        );
        this.platform = this
            .detect_platform()
            .await
            .context("failed detecting platform")?;
        log::info!("Remote platform discovered: {:?}", this.platform);
        this.os_version = this.detect_os_version().await;
        log::info!("Remote OS version discovered: {:?}", this.os_version);
        this.remote_binary_path = Some(
            this.ensure_server_binary(&delegate, release_channel, version, cx)
                .await
                .context("failed ensuring server binary")?,
        );
        if let Err(error) = this.ensure_slirp4netns_binary(&delegate, cx).await {
            super::warn_slirp4netns_not_installed(&error);
        }
        log::debug!("Detected WSL environment: {this:#?}");

        Ok(this)
    }

    async fn detect_platform(&self) -> Result<RemotePlatform> {
        let program = self.shell_kind.prepend_command_prefix("uname");
        let output = self.run_wsl_command_with_output(&program, &["-sm"]).await?;
        parse_platform(&output)
    }

    /// Best-effort detection of the remote OS version for telemetry. Failures
    /// result in `None` rather than failing the connection.
    async fn detect_os_version(&self) -> Option<String> {
        let (program, args) = super::os_version_command(self.platform.os);
        let program = self.shell_kind.prepend_command_prefix(program);
        match self.run_wsl_command_with_output(&program, args).await {
            Ok(output) => super::parse_os_version(self.platform.os, &output),
            Err(error) => {
                log::warn!("Failed to determine remote OS version: {error:#}");
                None
            }
        }
    }

    async fn detect_shell(&self) -> Result<String> {
        const DEFAULT_SHELL: &str = "sh";
        match self
            .run_wsl_command_with_output("sh", &["-c", "echo $SHELL"])
            .await
        {
            Ok(output) => Ok(parse_shell(&output, DEFAULT_SHELL)),
            Err(e) => {
                log::error!("Failed to detect remote shell: {e}");
                Ok(DEFAULT_SHELL.to_owned())
            }
        }
    }

    async fn detect_has_wsl_interop(&self) -> Result<bool> {
        let interop = match self
            .run_wsl_command_with_output("cat", &["/proc/sys/fs/binfmt_misc/WSLInterop"])
            .await
        {
            Ok(interop) => interop,
            Err(err) => self
                .run_wsl_command_with_output("cat", &["/proc/sys/fs/binfmt_misc/WSLInterop-late"])
                .await
                .inspect_err(|err2| log::error!("Failed to detect wsl interop: {err}; {err2}"))?,
        };
        Ok(interop.contains("enabled"))
    }

    async fn windows_path_to_wsl_path(&self, source: &Path) -> Result<String> {
        windows_path_to_wsl_path_impl(&self.connection_options, source).await
    }

    async fn run_wsl_command_with_output(&self, program: &str, args: &[&str]) -> Result<String> {
        run_wsl_command_with_output_impl(&self.connection_options, program, args).await
    }

    async fn run_wsl_command(&self, program: &str, args: &[&str]) -> Result<()> {
        run_wsl_command_impl(wsl_command_impl(
            &self.connection_options,
            program,
            args,
            false,
        ))
        .await
        .map(|_| ())
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: Version,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        let version_str = super::server_binary_version(release_channel, version);

        let binary_name = format!(
            "zed-remote-server-{}-{}",
            release_channel.dev_name(),
            version_str
        );

        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::from_unix_str(&binary_name).unwrap());

        if let Some(parent) = dst_path.parent() {
            let parent = parent.display(PathStyle::Unix);
            let mkdir = self.shell_kind.prepend_command_prefix("mkdir");
            self.run_wsl_command(&mkdir, &["-p", &parent])
                .await
                .map_err(|e| e.context("Failed to create directory"))?;
        }

        let binary_exists_on_server = self
            .run_wsl_command(&dst_path.display(PathStyle::Unix), &["version"])
            .await
            .is_ok();

        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        let locally_built = match super::build_remote_server_from_source(
            &self.platform,
            delegate.as_ref(),
            binary_exists_on_server,
            cx,
        )
        .await
        {
            Ok(locally_built) => locally_built,
            // A missing cross-compilation toolchain (zig, cargo-zigbuild,
            // cmake) must not be fatal: the bundled artifact is the fallback for
            // exactly that case.
            Err(error) => {
                log::warn!(
                    "failed to build the remote server from source, \
                     falling back to a bundled or downloaded binary: {error:#}"
                );
                None
            }
        };
        #[cfg(not(any(debug_assertions, feature = "build-remote-server-binary")))]
        let locally_built = None;

        let bundled_path = super::find_bundled_server_artifact(self.platform);
        let install_key = format!(
            "wsl:{}:{}|{}",
            self.connection_options.distro_name,
            self.connection_options.user.as_deref().unwrap_or_default(),
            dst_path.display(PathStyle::Unix)
        );
        // With nothing bundled to compare against, a binary that runs is all
        // there is to go on.
        let reusable_binary_on_server = binary_exists_on_server
            && (locally_built.is_some()
                || match &bundled_path {
                    None => true,
                    Some(bundled_path) => {
                        let installed = self.remote_file_digest(&dst_path).await;
                        super::installed_server_binary_is_verified(
                            &install_key,
                            bundled_path,
                            installed.as_ref(),
                        )
                        .await
                    }
                });

        match super::choose_server_binary_source(
            locally_built,
            reusable_binary_on_server,
            bundled_path,
        ) {
            super::ServerBinarySource::LocallyBuilt(remote_server_path) => {
                self.install_local_binary(&remote_server_path, &dst_path, delegate, cx)
                    .await
                    .context("installing locally built server binary")?;
                return Ok(dst_path.into());
            }
            super::ServerBinarySource::AlreadyInstalled => return Ok(dst_path.into()),
            super::ServerBinarySource::Bundled(bundled_path) => {
                log::info!(
                    "remote server binary source: artifact bundled with this app, {}",
                    bundled_path.display()
                );
                self.install_local_binary(&bundled_path, &dst_path, delegate, cx)
                    .await
                    .context("installing bundled server binary")?;
                let installed = self.remote_file_digest(&dst_path).await;
                super::record_verified_server_binary(&install_key, &bundled_path, installed).await;
                return Ok(dst_path.into());
            }
            super::ServerBinarySource::Download => {}
        }

        let wanted_version = match release_channel {
            ReleaseChannel::Nightly | ReleaseChannel::Dev => None,
            _ => Some(cx.update(|cx| AppVersion::global(cx))),
        };

        let src_path = delegate
            .download_server_binary_locally(self.platform, release_channel, wanted_version, cx)
            .await?;

        let tmp_path = format!(
            "{}.{}.gz",
            dst_path.display(PathStyle::Unix),
            std::process::id()
        );
        let tmp_path = RelPath::from_unix_str(&tmp_path).unwrap();

        self.upload_file(&src_path, &tmp_path, delegate, cx).await?;
        self.extract_and_install(&tmp_path, &dst_path, delegate, cx)
            .await?;

        Ok(dst_path.into())
    }

    /// ADR-F18: puts the bundled `slirp4netns` beside the server binary, where
    /// the sandbox helper looks for it. The helper runs it outside the sandbox,
    /// so a copy already there is kept only if it is byte for byte the bundled
    /// one (ADR-F46) — or, with nothing bundled to compare it against, if it
    /// runs at all.
    async fn ensure_slirp4netns_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let dst_path = super::remote_slirp4netns_path()?;
        let bundled_path = super::find_bundled_slirp4netns(self.platform);
        let installed_runs = self
            .run_wsl_command(&dst_path.display(PathStyle::Unix), &["--version"])
            .await
            .is_ok();
        if installed_runs {
            match &bundled_path {
                None => return Ok(()),
                Some(bundled_path) => {
                    if self.installed_file_matches(&dst_path, bundled_path).await {
                        return Ok(());
                    }
                }
            }
        }
        let Some(bundled_path) = bundled_path else {
            bail!("no slirp4netns bundled next to this executable");
        };
        log::info!(
            "installing bundled slirp4netns for the WSL sandbox: {}",
            bundled_path.display()
        );
        self.install_local_binary(&bundled_path, &dst_path, delegate, cx)
            .await
            .context("installing bundled slirp4netns")
    }

    /// The temporary upload keeps the source's file name, whose `.gz` extension
    /// is what `extract_and_install` decides to decompress on.
    async fn install_local_binary(
        &self,
        src_path: &Path,
        dst_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let tmp_path = super::remote_upload_temp_path(src_path)?;
        self.upload_file(src_path, &tmp_path, delegate, cx).await?;
        self.extract_and_install(&tmp_path, dst_path, delegate, cx)
            .await
    }

    /// Whether the distro's `path` holds exactly the bytes of the local `source`.
    async fn installed_file_matches(&self, path: &RelPath, source: &Path) -> bool {
        let local = match super::sha256_of_file(source).await {
            Ok(digest) => digest,
            Err(error) => {
                log::warn!("could not hash {}: {error:#}", source.display());
                return false;
            }
        };
        self.remote_file_digest(path)
            .await
            .is_some_and(|remote| remote.sha256 == local)
    }

    async fn remote_file_digest(&self, path: &RelPath) -> Option<super::RemoteFileDigest> {
        let path = path.display(PathStyle::Unix);
        match self
            .run_wsl_command_with_output(
                "sh",
                &["-c", super::REMOTE_FILE_DIGEST_SCRIPT, "zed-digest", &*path],
            )
            .await
        {
            Ok(output) => super::parse_remote_file_digest(&output),
            Err(error) => {
                log::debug!("could not hash {path} in WSL: {error:#}");
                None
            }
        }
    }

    /// Whether `program` is this connection's own server binary, which a
    /// terminal only ever runs as the ADR-F18 sandbox helper.
    fn is_sandbox_helper(&self, program: &str) -> bool {
        self.remote_binary_path
            .as_ref()
            .is_some_and(|path| path.display(PathStyle::Unix) == program)
    }

    /// ADR-F46: `wsl.exe --exec` starts the static helper itself, in the home
    /// directory its path is relative to, with no shell or `env` in front of it
    /// and none of the caller's environment: a dynamically linked program started
    /// first would load a project-controlled `LD_PRELOAD` outside the sandbox,
    /// and the helper reads `HOME` and `PATH` itself. The command's environment
    /// travels in its argv, as `--setenv`.
    fn build_sandbox_helper_command(
        &self,
        program: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<CommandTemplate> {
        anyhow::ensure!(
            env.is_empty(),
            "the sandbox helper takes the command's environment through --setenv"
        );
        let mut wsl_args = vec![
            "--distribution".to_string(),
            self.connection_options.distro_name.clone(),
        ];
        if let Some(user) = &self.connection_options.user {
            wsl_args.extend(["--user".to_string(), user.clone()]);
        }
        wsl_args.extend([
            "--cd".to_string(),
            "~".to_string(),
            "--exec".to_string(),
            program.to_string(),
        ]);
        wsl_args.extend(args.iter().cloned());
        Ok(CommandTemplate {
            program: "wsl.exe".to_string(),
            args: wsl_args,
            env: HashMap::default(),
        })
    }

    async fn upload_file(
        &self,
        src_path: &Path,
        dst_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some(i18n::t!("Uploading remote server")), cx);

        if let Some(parent) = dst_path.parent() {
            let parent = parent.display(PathStyle::Unix);
            let mkdir = self.shell_kind.prepend_command_prefix("mkdir");
            self.run_wsl_command(&mkdir, &["-p", &parent])
                .await
                .context("Failed to create directory when uploading file")?;
        }

        let t0 = Instant::now();
        let src_stat = fs::metadata(&src_path)
            .await
            .with_context(|| format!("source path does not exist: {}", src_path.display()))?;
        let size = src_stat.len();
        log::info!(
            "uploading remote server to WSL {:?} ({}kb)",
            dst_path,
            size / 1024
        );

        match self.copy_via_wslpath_and_cp(src_path, dst_path).await {
            Ok(()) => {}
            Err(cp_err) => {
                log::warn!(
                    "failed to upload remote server via /mnt, falling back to wsl.exe stdin: {cp_err:#}"
                );
                delegate.set_status(Some(i18n::t!("Streaming remote server into WSL")), cx);
                self.stream_file_into_wsl(src_path, dst_path)
                    .await
                    .with_context(|| {
                        format!("failed to stream file into WSL after /mnt copy failed: {cp_err:#}")
                    })?;
            }
        }

        log::info!("uploaded remote server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn copy_via_wslpath_and_cp(&self, src_path: &Path, dst_path: &RelPath) -> Result<()> {
        let src_path_in_wsl = self.windows_path_to_wsl_path(src_path).await?;
        let cp = self.shell_kind.prepend_command_prefix("cp");
        self.run_wsl_command(
            &cp,
            &["-f", &src_path_in_wsl, &dst_path.display(PathStyle::Unix)],
        )
        .await
        .map_err(|e| {
            anyhow!(
                "Failed to copy file {}({}) to WSL {:?}: {}",
                src_path.display(),
                src_path_in_wsl,
                dst_path,
                e
            )
        })
    }

    async fn stream_file_into_wsl(&self, src_path: &Path, dst_path: &RelPath) -> Result<()> {
        let mut file = fs::File::open(src_path).await.with_context(|| {
            format!(
                "failed to open {} for streaming into WSL",
                src_path.display()
            )
        })?;

        let dst_posix = dst_path.display(PathStyle::Unix);
        let mut command = wsl_command_impl(
            &self.connection_options,
            "sh",
            &["-c", "cat > \"$1\"", "zed-upload", dst_posix.as_ref()],
            true,
        );
        command.kill_on_drop(true);

        let mut child = command
            .spawn()
            .context("failed to spawn wsl.exe for stdin upload")?;
        let mut stdin = child
            .stdin
            .take()
            .context("wsl.exe child did not expose stdin")?;

        let copy_result = io::copy(&mut file, &mut stdin).await;
        let flush_result = stdin.flush().await;
        drop(stdin);

        let output = child
            .output()
            .await
            .context("failed to await wsl.exe stdin-upload child")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "wsl.exe stdin upload failed (status {:?}): {}",
                output.status.code(),
                stderr.trim()
            );
        }

        copy_result.with_context(|| {
            format!("failed to write {} into wsl.exe stdin", src_path.display())
        })?;
        flush_result.context("failed to flush wsl.exe stdin")?;

        Ok(())
    }

    async fn extract_and_install(
        &self,
        tmp_path: &RelPath,
        dst_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some(i18n::t!("Extracting remote server")), cx);

        let tmp_path_str = tmp_path.display(PathStyle::Unix);
        let dst_path_str = dst_path.display(PathStyle::Unix);

        // Build extraction script with proper error handling
        let script = if tmp_path_str.ends_with(".gz") {
            let uncompressed = tmp_path_str.trim_end_matches(".gz");
            format!(
                "set -e; gunzip -f '{}' && chmod 755 '{}' && mv -f '{}' '{}'",
                tmp_path_str, uncompressed, uncompressed, dst_path_str
            )
        } else {
            format!(
                "set -e; chmod 755 '{}' && mv -f '{}' '{}'",
                tmp_path_str, tmp_path_str, dst_path_str
            )
        };

        self.run_wsl_command("sh", &["-c", &script])
            .await
            .map_err(|e| e.context("Failed to extract server binary"))?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl RemoteConnection for WslRemoteConnection {
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        delegate.set_status(Some(i18n::t!("Starting proxy")), cx);

        let Some(remote_binary_path) = &self.remote_binary_path else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let mut proxy_args = vec![];
        for env_var in ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS"] {
            if let Some(value) = std::env::var(env_var).ok() {
                // We don't quote the value here as it seems excessive and may result in invalid envs for the
                // proxy server. For example, `RUST_LOG='debug'` will result in a warning "invalid logging spec 'debug'', ignoring it"
                // in the proxy server. Therefore, we pass the env vars as is.
                proxy_args.push(format!("{}={}", env_var, value));
            }
        }

        proxy_args.push(remote_binary_path.display(PathStyle::Unix).into_owned());
        proxy_args.push("proxy".to_owned());
        proxy_args.push("--identifier".to_owned());
        proxy_args.push(unique_identifier);

        if reconnect {
            proxy_args.push("--reconnect".to_owned());
        }

        let proxy_process =
            match wsl_command_impl(&self.connection_options, "env", &proxy_args, true)
                .kill_on_drop(true)
                .spawn()
            {
                Ok(process) => process,
                Err(error) => {
                    return Task::ready(Err(
                        anyhow::Error::new(error).context("failed to spawn remote server")
                    ));
                }
            };

        super::handle_rpc_messages_over_child_process_stdio(
            proxy_process,
            incoming_tx,
            outgoing_rx,
            connection_activity_tx,
            cx,
        )
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        cx.background_spawn({
            let options = self.connection_options.clone();
            async move {
                let wsl_src = windows_path_to_wsl_path_impl(&options, &src_path).await?;
                let mut command = wsl_command_impl(
                    &options,
                    "cp",
                    &["-r", &wsl_src, &dest_path.to_string()],
                    true,
                );
                command.kill_on_drop(true);
                run_wsl_command_impl(command).await.map_err(|e| {
                    anyhow!(
                        "failed to upload directory {} -> {}: {}",
                        src_path.display(),
                        dest_path,
                        e
                    )
                })?;

                Ok(())
            }
        })
    }

    async fn kill(&self) -> Result<()> {
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        false
    }

    fn shares_network_interface(&self) -> bool {
        true
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        _interactive: Interactive,
    ) -> Result<CommandTemplate> {
        if port_forward.is_some() {
            bail!("WSL shares the network interface with the host system");
        }

        if let Some(program) = &program
            && self.is_sandbox_helper(program)
        {
            return self.build_sandbox_helper_command(program, args, env);
        }

        let shell_kind = self.shell_kind;
        let working_dir = working_dir
            .map(|working_dir| RemotePathBuf::new(working_dir, PathStyle::Unix).to_string())
            .unwrap_or("~".to_string());

        let mut exec = String::from("exec env ");

        for (key, value) in env.iter() {
            let assignment = format!("{key}={value}");
            let assignment = shell_kind.try_quote(&assignment).context("shell quoting")?;
            write!(exec, "{assignment} ")?;
        }

        if let Some(program) = program {
            write!(
                exec,
                "{}",
                shell_kind
                    .try_quote_prefix_aware(&program)
                    .context("shell quoting")?
            )?;
            for arg in args {
                let arg = shell_kind.try_quote(&arg).context("shell quoting")?;
                write!(exec, " {arg}")?;
            }
        } else {
            write!(&mut exec, "{} -l", self.shell)?;
        }
        let (command, args) =
            ShellBuilder::new(&Shell::Program(self.shell.clone()), false).build(Some(exec), &[]);

        let mut wsl_args = if let Some(user) = &self.connection_options.user {
            vec![
                "--distribution".to_string(),
                self.connection_options.distro_name.clone(),
                "--user".to_string(),
                user.clone(),
                "--cd".to_string(),
                working_dir,
                "--".to_string(),
                command,
            ]
        } else {
            vec![
                "--distribution".to_string(),
                self.connection_options.distro_name.clone(),
                "--cd".to_string(),
                working_dir,
                "--".to_string(),
                command,
            ]
        };
        wsl_args.extend(args);

        Ok(CommandTemplate {
            program: "wsl.exe".to_string(),
            args: wsl_args,
            env: HashMap::default(),
        })
    }

    fn build_forward_ports_command(
        &self,
        _: Vec<(u16, String, u16)>,
    ) -> anyhow::Result<CommandTemplate> {
        Err(anyhow!("WSL shares a network interface with the host"))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Wsl(self.connection_options.clone())
    }

    fn path_style(&self) -> PathStyle {
        PathStyle::Unix
    }

    fn remote_platform(&self) -> RemotePlatform {
        self.platform
    }

    fn remote_os_version(&self) -> Option<String> {
        self.os_version.clone()
    }

    fn shell(&self) -> String {
        self.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.default_system_shell.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        self.has_wsl_interop
    }

    // ADR-F46: only a connection that names its server binary gets agent
    // commands wrapped by the ADR-F18 helper; without this they ran in the
    // distro unwrapped.
    fn remote_server_binary_path(&self) -> Option<Arc<RelPath>> {
        self.remote_binary_path.clone()
    }
}

/// `wslpath` is a executable available in WSL, it's a linux binary.
/// So it doesn't support Windows style paths.
async fn sanitize_path(path: &Path) -> Result<String> {
    let path = smol::fs::canonicalize(path)
        .await
        .with_context(|| format!("Failed to canonicalize path {}", path.display()))?;
    let path_str = path.to_string_lossy();

    let sanitized = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);
    Ok(sanitized.replace('\\', "/"))
}

fn run_wsl_command_with_output_impl(
    options: &WslConnectionOptions,
    program: &str,
    args: &[&str],
) -> impl Future<Output = Result<String>> + use<> {
    let exec_command = wsl_command_impl(options, program, args, true);
    let command = wsl_command_impl(options, program, args, false);
    async move {
        match run_wsl_command_impl(exec_command).await {
            Ok(res) => Ok(res),
            Err(exec_err) => match run_wsl_command_impl(command).await {
                Ok(res) => Ok(res),
                Err(e) => Err(e.context(exec_err)),
            },
        }
    }
}

impl WslConnectionOptions {
    pub fn abs_windows_path_to_wsl_path(
        &self,
        source: &Path,
    ) -> impl Future<Output = Result<String>> + use<> {
        let path_str = source.to_string_lossy();

        let source = path_str.strip_prefix(r"\\?\").unwrap_or(&*path_str);
        let source = source.replace('\\', "/");
        run_wsl_command_with_output_impl(self, "wslpath", &["-u", &source])
    }
}

async fn windows_path_to_wsl_path_impl(
    options: &WslConnectionOptions,
    source: &Path,
) -> Result<String> {
    let source = sanitize_path(source).await?;
    run_wsl_command_with_output_impl(options, "wslpath", &["-u", &source]).await
}

/// Converts a WSL/POSIX path to a Windows path using `wslpath -w`.
///
/// For example, `/home/user/project` becomes `\\wsl.localhost\Ubuntu\home\user\project`
#[cfg(target_os = "windows")]
pub fn wsl_path_to_windows_path(
    options: &WslConnectionOptions,
    wsl_path: &Path,
) -> impl Future<Output = Result<PathBuf>> + use<> {
    let wsl_path_str = wsl_path.to_string_lossy().to_string();
    let command = wsl_command_impl(options, "wslpath", &["-w", &wsl_path_str], true);
    async move {
        let windows_path = run_wsl_command_impl(command).await?;
        Ok(PathBuf::from(windows_path))
    }
}

fn run_wsl_command_impl(
    mut command: util::command::Command,
) -> impl Future<Output = Result<String>> {
    async move {
        let output = command
            .output()
            .await
            .with_context(|| format!("Failed to run command '{:?}'", command))?;

        if !output.status.success() {
            return Err(anyhow!(
                "Command '{:?}' failed: {}",
                command,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

/// Creates a new `wsl.exe` command that runs the given program with the given arguments.
///
/// If `exec` is true, the command will be executed in the WSL environment without spawning a new shell.
fn wsl_command_impl(
    options: &WslConnectionOptions,
    program: &str,
    args: &[impl AsRef<OsStr>],
    exec: bool,
) -> util::command::Command {
    let mut command = util::command::new_command("wsl.exe");

    if let Some(user) = &options.user {
        command.arg("--user").arg(user);
    }

    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--distribution")
        .arg(&options.distro_name)
        .arg("--cd")
        .arg("~");

    if exec {
        command.arg("--exec");
    }

    command.arg(program).args(args);

    log::debug!("wsl {:?}", command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_with_server_binary(
        remote_binary_path: Option<Arc<RelPath>>,
    ) -> WslRemoteConnection {
        WslRemoteConnection {
            remote_binary_path,
            platform: RemotePlatform {
                os: RemoteOs::Linux,
                arch: RemoteArch::X86_64,
            },
            os_version: None,
            shell: "/bin/bash".to_string(),
            shell_kind: ShellKind::Posix,
            default_system_shell: "/bin/sh".to_string(),
            has_wsl_interop: false,
            connection_options: WslConnectionOptions {
                distro_name: "Ubuntu".to_string(),
                user: None,
            },
        }
    }

    #[test]
    fn test_remote_server_binary_path_is_exposed_for_the_sandbox_helper() {
        assert_eq!(
            connection_with_server_binary(None).remote_server_binary_path(),
            None
        );

        let installed: Arc<RelPath> = paths::remote_server_dir_relative()
            .join(RelPath::from_unix_str("zed-remote-server-stable-1.17.2").unwrap())
            .into();
        assert_eq!(
            connection_with_server_binary(Some(installed.clone())).remote_server_binary_path(),
            Some(installed)
        );
    }

    #[test]
    fn test_sandbox_helper_is_executed_directly() {
        let helper = ".zed_server/zed-remote-server-stable-1.17.2";
        let installed: Arc<RelPath> = RelPath::from_unix_str(helper).unwrap().into();
        let connection = connection_with_server_binary(Some(installed));
        let args = vec![
            "--remote-sandbox-helper".to_string(),
            "--".to_string(),
            "/bin/sh".to_string(),
        ];

        let command = connection
            .build_command(
                Some(helper.to_string()),
                &args,
                &HashMap::default(),
                None,
                None,
                Interactive::Yes,
            )
            .unwrap();
        assert_eq!(command.program, "wsl.exe");
        assert_eq!(
            command.args,
            [
                "--distribution",
                "Ubuntu",
                "--cd",
                "~",
                "--exec",
                helper,
                "--remote-sandbox-helper",
                "--",
                "/bin/sh"
            ]
        );
        assert!(command.env.is_empty());

        // On the helper's own process an environment would act before the
        // sandbox exists; it has to come through `--setenv` instead.
        let environment = HashMap::from_iter([("LD_PRELOAD".to_string(), "x.so".to_string())]);
        assert!(
            connection
                .build_command(
                    Some(helper.to_string()),
                    &args,
                    &environment,
                    None,
                    None,
                    Interactive::Yes
                )
                .is_err()
        );

        let command = connection
            .build_command(
                Some("ls".to_string()),
                &[],
                &HashMap::default(),
                None,
                None,
                Interactive::Yes,
            )
            .unwrap();
        assert!(
            !command.args.iter().any(|arg| arg == "--exec"),
            "any other program still goes through the user's shell"
        );
    }
}
