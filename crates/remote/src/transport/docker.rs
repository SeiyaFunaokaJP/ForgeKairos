use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use collections::HashMap;
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use semver::Version as SemanticVersion;
use std::collections::BTreeMap;
use std::time::Instant;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::command::Stdio;
use util::shell::ShellKind;
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
};

use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext, AsyncApp, Task};
use rpc::proto::Envelope;

use crate::{
    RemoteArch, RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions, RemoteOs,
    RemotePlatform,
    remote_client::{CommandTemplate, Interactive},
    transport::parse_platform,
};

#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct DockerConnectionOptions {
    pub name: String,
    pub container_id: String,
    pub remote_user: String,
    pub upload_binary_over_docker_exec: bool,
    pub use_podman: bool,
    pub remote_env: BTreeMap<String, String>,
}

pub(crate) struct DockerExecConnection {
    proxy_process: Mutex<Option<u32>>,
    remote_dir_for_server: String,
    remote_binary_relpath: Option<Arc<RelPath>>,
    connection_options: DockerConnectionOptions,
    remote_platform: Option<RemotePlatform>,
    os_version: Option<String>,
    path_style: Option<PathStyle>,
    shell: String,
}

impl DockerExecConnection {
    pub async fn new(
        connection_options: DockerConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let mut this = Self {
            proxy_process: Mutex::new(None),
            remote_dir_for_server: "/".to_string(),
            remote_binary_relpath: None,
            connection_options,
            remote_platform: None,
            os_version: None,
            path_style: None,
            shell: "sh".to_owned(),
        };
        let (release_channel, version, commit) = cx.update(|cx| {
            (
                ReleaseChannel::global(cx),
                AppVersion::global(cx),
                AppCommitSha::try_global(cx),
            )
        });
        let remote_platform = this.check_remote_platform().await?;

        this.path_style = match remote_platform.os {
            RemoteOs::Windows => Some(PathStyle::Windows),
            _ => Some(PathStyle::Unix),
        };

        this.remote_platform = Some(remote_platform);
        log::info!("Remote platform discovered: {:?}", this.remote_platform);

        this.os_version = this.discover_os_version(remote_platform.os).await;
        log::info!("Remote OS version discovered: {:?}", this.os_version);

        this.shell = this.discover_shell().await;
        log::info!("Remote shell discovered: {}", this.shell);

        this.remote_dir_for_server = this.docker_user_home_dir().await?.trim().to_string();

        this.remote_binary_relpath = Some(
            this.ensure_server_binary(
                &delegate,
                release_channel,
                version,
                &this.remote_dir_for_server,
                commit,
                cx,
            )
            .await?,
        );

        if let Err(error) = this.ensure_slirp4netns_binary(&delegate, cx).await {
            super::warn_slirp4netns_not_installed(&error);
        }

        Ok(this)
    }

    fn docker_cli(&self) -> &str {
        if self.connection_options.use_podman {
            "podman"
        } else {
            "docker"
        }
    }

    /// Run a shell command inside the container and reliably extract its output
    /// using unique delimiters, so that shell initialization noise (e.g. from
    /// BASH_ENV or .bashrc) does not corrupt the result.
    async fn run_docker_exec_delimited(&self, script: &str) -> Result<String> {
        const MARKER: &str = "=====ZED_DELIM_7f3a9c=====";
        let wrapped =
            format!("printf '{MARKER}'; {script}; __exit=$?; printf '{MARKER}'; exit $__exit");
        let output = self
            .run_docker_exec("sh", None, &Default::default(), &["-c", &wrapped])
            .await?;
        let start = output.find(MARKER).map(|i| i + MARKER.len()).unwrap_or(0);
        let end = output[start..]
            .find(MARKER)
            .map(|i| start + i)
            .unwrap_or(output.len());
        Ok(output[start..end].to_string())
    }

    async fn discover_shell(&self) -> String {
        let default_shell = "sh";
        match self.run_docker_exec_delimited("echo $SHELL").await {
            Ok(shell) => match shell.trim() {
                "" => {
                    log::info!("$SHELL is not set, checking passwd for user");
                }
                shell => {
                    return shell.to_owned();
                }
            },
            Err(e) => {
                log::error!("Failed to get $SHELL: {e}. Checking passwd for user");
            }
        }

        match self
            .run_docker_exec_delimited("getent passwd \"$(id -un)\" | cut -d: -f7")
            .await
        {
            Ok(shell) => match shell.trim() {
                "" => {
                    log::info!("No shell found in passwd, falling back to {default_shell}");
                }
                shell => {
                    return shell.to_owned();
                }
            },
            Err(e) => {
                log::info!("Error getting shell from passwd: {e}. Falling back to {default_shell}");
            }
        }
        default_shell.to_owned()
    }

    async fn check_remote_platform(&self) -> Result<RemotePlatform> {
        let uname = self.run_docker_exec_delimited("uname -sm").await?;
        parse_platform(&uname)
    }

    /// Best-effort detection of the container's OS version for telemetry.
    async fn discover_os_version(&self, os: RemoteOs) -> Option<String> {
        let (program, args) = super::os_version_command(os);
        match self
            .run_docker_exec(program, None, &Default::default(), args)
            .await
        {
            Ok(output) => super::parse_os_version(os, &output),
            Err(error) => {
                log::warn!("Failed to determine remote OS version: {error:#}");
                None
            }
        }
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: SemanticVersion,
        remote_dir_for_server: &str,
        commit: Option<AppCommitSha>,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        let remote_platform = self
            .remote_platform
            .context("No remote platform defined; cannot proceed.")?;

        let version_str = match release_channel {
            ReleaseChannel::Nightly => {
                let commit = commit.map(|s| s.full()).unwrap_or_default();
                format!("{}-{}", version, commit)
            }
            ReleaseChannel::Dev => "build".to_string(),
            _ => version.to_string(),
        };
        let binary_name = format!(
            "zed-remote-server-{}-{}",
            release_channel.dev_name(),
            version_str
        );
        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::from_unix_str(&binary_name).unwrap());

        let binary_exists_on_server = self
            .run_docker_exec(
                &dst_path.display(self.path_style()),
                Some(&remote_dir_for_server),
                &Default::default(),
                &["version"],
            )
            .await
            .is_ok();
        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        let locally_built = match super::build_remote_server_from_source(
            &remote_platform,
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

        let bundled_path = super::find_bundled_server_artifact(remote_platform);
        let install_key = format!(
            "{}:{}:{}|{}",
            self.docker_cli(),
            self.connection_options.container_id,
            self.connection_options.remote_user,
            dst_path.display(self.path_style())
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
                self.install_local_binary(
                    &remote_server_path,
                    &dst_path,
                    remote_dir_for_server,
                    delegate,
                    cx,
                )
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
                self.install_local_binary(
                    &bundled_path,
                    &dst_path,
                    remote_dir_for_server,
                    delegate,
                    cx,
                )
                .await
                .context("installing bundled server binary")?;
                let installed = self.remote_file_digest(&dst_path).await;
                super::record_verified_server_binary(&install_key, &bundled_path, installed).await;
                return Ok(dst_path.into());
            }
            super::ServerBinarySource::Download => {}
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => {
                anyhow::bail!(
                    "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at ({:?})",
                    dst_path
                )
            }
            _ => Ok(Some(AppVersion::global(cx))),
        })?;

        let tmp_path_gz = paths::remote_server_dir_relative().join(
            RelPath::from_unix_str(&format!(
                "{}-download-{}.gz",
                binary_name,
                std::process::id()
            ))
            .unwrap(),
        );
        if !self.connection_options.upload_binary_over_docker_exec
            && let Some(url) = delegate
                .get_download_url(remote_platform, release_channel, wanted_version.clone(), cx)
                .await?
        {
            match self
                .download_binary_on_server(&url, &tmp_path_gz, &remote_dir_for_server, delegate, cx)
                .await
            {
                Ok(_) => {
                    self.extract_server_binary(
                        &dst_path,
                        &tmp_path_gz,
                        &remote_dir_for_server,
                        delegate,
                        cx,
                    )
                    .await
                    .context("extracting server binary")?;
                    return Ok(dst_path.into());
                }
                Err(e) => {
                    log::error!(
                        "Failed to download binary on server, attempting to download locally and then upload it the server: {e:#}",
                    )
                }
            }
        }

        let src_path = delegate
            .download_server_binary_locally(remote_platform, release_channel, wanted_version, cx)
            .await
            .context("downloading server binary locally")?;
        self.upload_local_server_binary(
            &src_path,
            &tmp_path_gz,
            &remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("uploading server binary")?;
        self.extract_server_binary(
            &dst_path,
            &tmp_path_gz,
            &remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("extracting server binary")?;
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
        let remote_platform = self
            .remote_platform
            .context("No remote platform defined; cannot proceed.")?;
        if remote_platform.os != RemoteOs::Linux {
            return Ok(());
        }
        let remote_dir_for_server = self.remote_dir_for_server.as_str();
        let dst_path = super::remote_slirp4netns_path()?;
        let bundled_path = super::find_bundled_slirp4netns(remote_platform);
        let installed_runs = self
            .run_docker_exec(
                &dst_path.display(self.path_style()),
                Some(remote_dir_for_server),
                &Default::default(),
                &["--version"],
            )
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
            anyhow::bail!("no slirp4netns bundled next to this executable");
        };
        log::info!(
            "installing bundled slirp4netns for the container sandbox: {}",
            bundled_path.display()
        );
        self.install_local_binary(
            &bundled_path,
            &dst_path,
            remote_dir_for_server,
            delegate,
            cx,
        )
        .await
        .context("installing bundled slirp4netns")
    }

    /// The temporary upload keeps the source's file name, whose `.gz` extension
    /// is what `extract_server_binary` decides to decompress on.
    async fn install_local_binary(
        &self,
        src_path: &Path,
        dst_path: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let tmp_path = super::remote_upload_temp_path(src_path)?;
        self.upload_local_server_binary(src_path, &tmp_path, remote_dir_for_server, delegate, cx)
            .await?;
        self.extract_server_binary(dst_path, &tmp_path, remote_dir_for_server, delegate, cx)
            .await
    }

    /// Whether the container's `path` holds exactly the bytes of the local
    /// `source`.
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
        let path = path.display(self.path_style());
        match self
            .run_docker_exec(
                "sh",
                Some(self.remote_dir_for_server.as_str()),
                &Default::default(),
                &["-c", super::REMOTE_FILE_DIGEST_SCRIPT, "zed-digest", &*path],
            )
            .await
        {
            Ok(output) => super::parse_remote_file_digest(&output),
            Err(error) => {
                log::debug!("could not hash {path} in the container: {error:#}");
                None
            }
        }
    }

    /// Whether `program` is this connection's own server binary, which a
    /// terminal only ever runs as the ADR-F18 sandbox helper.
    fn is_sandbox_helper(&self, program: &str) -> bool {
        self.remote_binary_relpath
            .as_ref()
            .is_some_and(|path| path.display(self.path_style()) == program)
    }

    /// ADR-F46: `docker exec` starts the static helper itself, by absolute path
    /// because without `-w` the working directory is the container's rather than
    /// the home the path is relative to, and with neither the caller's
    /// environment nor the container's `remoteEnv`: both come from the project,
    /// and on the helper's own process they would act before the sandbox exists
    /// (it reads `HOME` and `PATH` itself). The command's environment travels in
    /// its argv, as `--setenv`.
    fn build_sandbox_helper_command(
        &self,
        program: &str,
        args: &[String],
        env: &HashMap<String, String>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        anyhow::ensure!(
            env.is_empty(),
            "the sandbox helper takes the command's environment through --setenv"
        );
        let helper = format!(
            "{}/{program}",
            self.remote_dir_for_server.trim_end_matches('/')
        );
        let mut docker_args = vec![
            "exec".to_string(),
            "-u".to_string(),
            self.connection_options.remote_user.clone(),
        ];
        docker_args.push(
            match interactive {
                Interactive::Yes => "-it",
                Interactive::No => "-i",
            }
            .to_string(),
        );
        docker_args.push(self.connection_options.container_id.clone());
        docker_args.push(helper);
        docker_args.extend(args.iter().cloned());
        Ok(CommandTemplate {
            program: self.docker_cli().to_string(),
            args: docker_args,
            env: Default::default(),
        })
    }

    async fn docker_user_home_dir(&self) -> Result<String> {
        self.run_docker_exec_delimited("echo $HOME").await
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some("リモート開発サーバーを展開しています"), cx);
        let script = install_uploaded_binary_script(tmp_path, dst_path, self.path_style())?;
        let args = ShellKind::Posix.args_for_shell(false, script);
        self.run_docker_exec(
            "sh",
            Some(&remote_dir_for_server),
            &Default::default(),
            &args,
        )
        .await
        .context("unpacking the uploaded binary into place")?;
        Ok(())
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path_gz: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path_gz.parent() {
            self.run_docker_exec(
                "mkdir",
                Some(remote_dir_for_server),
                &Default::default(),
                &["-p", parent.display(self.path_style()).as_ref()],
            )
            .await?;
        }

        let src_stat = smol::fs::metadata(&src_path).await?;
        let size = src_stat.len();

        let t0 = Instant::now();
        delegate.set_status(Some("リモート開発サーバーを転送しています"), cx);
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path_gz,
            size / 1024
        );
        self.upload_file(src_path, tmp_path_gz, remote_dir_for_server)
            .await
            .context("failed to upload server binary")?;
        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn upload_and_chown(
        docker_cli: String,
        connection_options: DockerConnectionOptions,
        src_path: String,
        dst_path: String,
    ) -> Result<()> {
        let mut command = util::command::new_command(&docker_cli);
        command.arg("cp");
        command.arg("-a");
        command.arg(&src_path);
        command.arg(format!("{}:{}", connection_options.container_id, dst_path));

        let output = command.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("failed to upload via docker cp {src_path} -> {dst_path}: {stderr}",);
            anyhow::bail!(
                "failed to upload via docker cp {} -> {}: {}",
                src_path,
                dst_path,
                stderr,
            );
        }

        let mut chown_command = util::command::new_command(&docker_cli);
        chown_command.arg("exec");
        chown_command.arg(connection_options.container_id);
        chown_command.arg("chown");
        chown_command.arg(format!(
            "{}:{}",
            connection_options.remote_user, connection_options.remote_user,
        ));
        chown_command.arg(&dst_path);

        let output = chown_command.output().await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!("failed to change ownership for via chown: {stderr}",);
        anyhow::bail!(
            "failed to change ownership for zed_remote_server via chown: {}",
            stderr,
        );
    }

    async fn upload_file(
        &self,
        src_path: &Path,
        dest_path: &RelPath,
        remote_dir_for_server: &str,
    ) -> Result<()> {
        log::debug!("uploading file {:?} to {:?}", src_path, dest_path);

        let src_path_display = src_path.display().to_string();
        let dest_path_str = dest_path.display(self.path_style());
        let full_server_path = format!("{}/{}", remote_dir_for_server, dest_path_str);

        Self::upload_and_chown(
            self.docker_cli().to_string(),
            self.connection_options.clone(),
            src_path_display,
            full_server_path,
        )
        .await
    }

    async fn run_docker_command(
        &self,
        subcommand: &str,
        args: &[impl AsRef<str>],
    ) -> Result<String> {
        let mut command = util::command::new_command(self.docker_cli());
        command.arg(subcommand);
        for arg in args {
            command.arg(arg.as_ref());
        }
        let output = command.output().await?;
        log::debug!("{:?}: {:?}", command, output);
        anyhow::ensure!(
            output.status.success(),
            "failed to run command {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn run_docker_exec(
        &self,
        inner_program: &str,
        working_directory: Option<&str>,
        env: &HashMap<String, String>,
        program_args: &[impl AsRef<str>],
    ) -> Result<String> {
        let mut args = match working_directory {
            Some(dir) => vec!["-w".to_string(), dir.to_string()],
            None => vec![],
        };

        args.push("-u".to_string());
        args.push(self.connection_options.remote_user.clone());

        for (k, v) in self.connection_options.remote_env.iter() {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }

        for (k, v) in env.iter() {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }

        args.push(self.connection_options.container_id.clone());
        args.push(inner_program.to_string());

        for arg in program_args {
            args.push(arg.as_ref().to_owned());
        }
        self.run_docker_command("exec", args.as_ref()).await
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path_gz: &RelPath,
        remote_dir_for_server: &str,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path_gz.parent() {
            self.run_docker_exec(
                "mkdir",
                Some(remote_dir_for_server),
                &Default::default(),
                &["-p", parent.display(self.path_style()).as_ref()],
            )
            .await?;
        }

        delegate.set_status(
            Some("ホスト上でリモート開発サーバーをダウンロードしています"),
            cx,
        );

        match self
            .run_docker_exec(
                "curl",
                Some(remote_dir_for_server),
                &Default::default(),
                &[
                    "-f",
                    "-L",
                    url,
                    "-o",
                    &tmp_path_gz.display(self.path_style()),
                ],
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if self
                    .run_docker_exec("which", None, &Default::default(), &["curl"])
                    .await
                    .is_ok()
                {
                    return Err(e);
                }

                log::info!("curl is not available, trying wget");
                match self
                    .run_docker_exec(
                        "wget",
                        Some(remote_dir_for_server),
                        &Default::default(),
                        &[url, "-O", &tmp_path_gz.display(self.path_style())],
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        if self
                            .run_docker_exec("which", None, &Default::default(), &["wget"])
                            .await
                            .is_ok()
                        {
                            return Err(e);
                        } else {
                            anyhow::bail!("Neither curl nor wget is available");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn kill_inner(&self) -> Result<()> {
        if let Some(pid) = self.proxy_process.lock().take() {
            if let Ok(_) = util::command::new_command("kill")
                .arg(pid.to_string())
                .spawn()
            {
                Ok(())
            } else {
                Err(anyhow::anyhow!("Failed to kill process"))
            }
        } else {
            Ok(())
        }
    }
}

#[async_trait(?Send)]
impl RemoteConnection for DockerExecConnection {
    fn has_wsl_interop(&self) -> bool {
        false
    }
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
        // We'll try connecting anew every time we open a devcontainer, so proactively try to kill any old connections.
        if !self.has_been_killed() {
            if let Err(e) = self.kill_inner() {
                return Task::ready(Err(e));
            };
        }

        delegate.set_status(Some("プロキシを起動しています"), cx);

        let Some(remote_binary_relpath) = self.remote_binary_relpath.clone() else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let mut docker_args = vec!["exec".to_string()];

        for (k, v) in self.connection_options.remote_env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{k}={v}"));
        }
        for env_var in ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS"] {
            if let Some(value) = std::env::var(env_var).ok() {
                docker_args.push("-e".to_string());
                docker_args.push(format!("{env_var}={value}"));
            }
        }

        docker_args.extend([
            "-u".to_string(),
            self.connection_options.remote_user.to_string(),
            "-w".to_string(),
            self.remote_dir_for_server.clone(),
            "-i".to_string(),
            self.connection_options.container_id.to_string(),
        ]);

        let val = remote_binary_relpath
            .display(self.path_style())
            .into_owned();
        docker_args.push(val);
        docker_args.push("proxy".to_string());
        docker_args.push("--identifier".to_string());
        docker_args.push(unique_identifier);
        if reconnect {
            docker_args.push("--reconnect".to_string());
        }
        let mut command = util::command::new_command(self.docker_cli());
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(docker_args);

        let Ok(child) = command.spawn() else {
            return Task::ready(Err(anyhow::anyhow!(
                "Failed to start remote server process"
            )));
        };

        let mut proxy_process = self.proxy_process.lock();
        *proxy_process = Some(child.id());

        cx.spawn(async move |cx| {
            super::handle_rpc_messages_over_child_process_stdio(
                child,
                incoming_tx,
                outgoing_rx,
                connection_activity_tx,
                cx,
            )
            .await
            .and_then(|status| {
                if status != 0 {
                    anyhow::bail!("Remote server exited with status {status}");
                }
                Ok(0)
            })
        })
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let dest_path_str = dest_path.to_string();
        let src_path_display = src_path.display().to_string();

        let upload_task = Self::upload_and_chown(
            self.docker_cli().to_string(),
            self.connection_options.clone(),
            src_path_display,
            dest_path_str,
        );

        cx.background_spawn(upload_task)
    }

    async fn kill(&self) -> Result<()> {
        self.kill_inner()
    }

    fn has_been_killed(&self) -> bool {
        self.proxy_process.lock().is_none()
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        _port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        if let Some(program) = &program
            && self.is_sandbox_helper(program)
        {
            return self.build_sandbox_helper_command(program, args, env, interactive);
        }

        let mut parsed_working_dir = None;

        let path_style = self.path_style();

        if let Some(working_dir) = working_dir {
            let working_dir = RemotePathBuf::new(working_dir, path_style).to_string();

            const TILDE_PREFIX: &'static str = "~/";
            if working_dir.starts_with(TILDE_PREFIX) {
                let working_dir = working_dir.trim_start_matches("~").trim_start_matches("/");
                parsed_working_dir =
                    Some(format!("{}/{}", self.remote_dir_for_server, working_dir));
            } else {
                parsed_working_dir = Some(working_dir);
            }
        }

        let mut inner_program = Vec::new();

        if let Some(program) = program {
            inner_program.push(program);
            for arg in args {
                inner_program.push(arg.clone());
            }
        } else {
            inner_program.push(self.shell());
            inner_program.push("-l".to_string());
        };

        let mut docker_args = vec![
            "exec".to_string(),
            "-u".to_string(),
            self.connection_options.remote_user.clone(),
        ];

        if let Some(parsed_working_dir) = parsed_working_dir {
            docker_args.push("-w".to_string());
            docker_args.push(parsed_working_dir);
        }

        for (k, v) in self.connection_options.remote_env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{k}={v}"));
        }

        for (k, v) in env.iter() {
            docker_args.push("-e".to_string());
            docker_args.push(format!("{k}={v}"));
        }

        match interactive {
            Interactive::Yes => docker_args.push("-it".to_string()),
            Interactive::No => docker_args.push("-i".to_string()),
        }
        docker_args.push(self.connection_options.container_id.to_string());

        docker_args.append(&mut inner_program);

        Ok(CommandTemplate {
            program: self.docker_cli().to_string(),
            args: docker_args,
            // Docker-exec pipes in environment via the "-e" argument
            env: Default::default(),
        })
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        Err(anyhow::anyhow!("Not currently supported for docker_exec"))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Docker(self.connection_options.clone())
    }

    fn path_style(&self) -> PathStyle {
        self.path_style.unwrap_or(PathStyle::Unix)
    }

    fn remote_platform(&self) -> RemotePlatform {
        // Docker containers are always Linux; the platform is populated during
        // setup, so this fallback is only for the brief pre-detection window.
        self.remote_platform.unwrap_or(RemotePlatform {
            os: RemoteOs::Linux,
            arch: RemoteArch::X86_64,
        })
    }

    fn remote_os_version(&self) -> Option<String> {
        self.os_version.clone()
    }

    fn shell(&self) -> String {
        self.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        String::from("/bin/sh")
    }

    // ADR-F46: exposed even though Docker's default seccomp profile allows
    // `unshare` only with CAP_SYS_ADMIN, so in most containers the ADR-F18
    // helper refuses to start (exit 126). Refusing is the point: without this
    // path, agent commands ran in the container unwrapped.
    fn remote_server_binary_path(&self) -> Option<Arc<RelPath>> {
        self.remote_binary_relpath.clone()
    }
}

/// Unpacks an uploaded binary (gunzipping it first when it is a `.gz`), makes it
/// executable and renames it over `dst_path`, so nothing ever runs a
/// half-written file.
fn install_uploaded_binary_script(
    tmp_path: &RelPath,
    dst_path: &RelPath,
    path_style: PathStyle,
) -> Result<String> {
    let shell_kind = ShellKind::Posix;
    let tmp_path = tmp_path.display(path_style);
    let uploaded = shell_kind.try_quote(&tmp_path).context("shell quoting")?;
    let dst_path = dst_path.display(path_style);
    let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;
    Ok(match tmp_path.strip_suffix(".gz") {
        Some(unpacked) => {
            let unpacked = shell_kind.try_quote(unpacked).context("shell quoting")?;
            format!("gunzip -f {uploaded} && chmod 755 {unpacked} && mv {unpacked} {dst_path}")
        }
        None => format!("chmod 755 {uploaded} && mv {uploaded} {dst_path}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_with_server_binary(
        remote_binary_relpath: Option<Arc<RelPath>>,
    ) -> DockerExecConnection {
        DockerExecConnection {
            proxy_process: Mutex::new(None),
            remote_dir_for_server: "/home/vscode".to_string(),
            remote_binary_relpath,
            connection_options: DockerConnectionOptions::default(),
            remote_platform: Some(RemotePlatform {
                os: RemoteOs::Linux,
                arch: RemoteArch::X86_64,
            }),
            os_version: None,
            path_style: Some(PathStyle::Unix),
            shell: "/bin/bash".to_string(),
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
        let mut connection = connection_with_server_binary(Some(installed));
        connection.connection_options.container_id = "container".to_string();
        connection.connection_options.remote_user = "vscode".to_string();
        connection.connection_options.remote_env = BTreeMap::from_iter([(
            "LD_PRELOAD".to_string(),
            "/workspaces/project/x.so".to_string(),
        )]);
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
                Some("/workspaces/project".to_string()),
                None,
                Interactive::Yes,
            )
            .unwrap();
        assert_eq!(command.program, "docker");
        assert_eq!(
            command.args,
            [
                "exec",
                "-u",
                "vscode",
                "-it",
                "container",
                "/home/vscode/.zed_server/zed-remote-server-stable-1.17.2",
                "--remote-sandbox-helper",
                "--",
                "/bin/sh"
            ],
            "no `-w`, no `-e`: the container's `remoteEnv` comes from the project"
        );

        let environment = HashMap::from_iter([("A".to_string(), "B".to_string())]);
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
                Interactive::No,
            )
            .unwrap();
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg == "LD_PRELOAD=/workspaces/project/x.so"),
            "any other program keeps the container's `remoteEnv`"
        );
    }

    #[test]
    fn test_install_uploaded_binary_script() {
        // The bundled server artifact is gzipped.
        let script = install_uploaded_binary_script(
            RelPath::from_unix_str(".zed_server/download-7-zed-remote-server-linux-x86_64.gz")
                .unwrap(),
            RelPath::from_unix_str(".zed_server/zed-remote-server-stable-1.17.2").unwrap(),
            PathStyle::Unix,
        )
        .unwrap();
        assert_eq!(
            script,
            "gunzip -f .zed_server/download-7-zed-remote-server-linux-x86_64.gz \
             && chmod 755 .zed_server/download-7-zed-remote-server-linux-x86_64 \
             && mv .zed_server/download-7-zed-remote-server-linux-x86_64 \
             .zed_server/zed-remote-server-stable-1.17.2"
        );

        // slirp4netns ships uncompressed.
        let script = install_uploaded_binary_script(
            RelPath::from_unix_str(".zed_server/download-7-slirp4netns-linux-x86_64").unwrap(),
            RelPath::from_unix_str(".zed_server/slirp4netns").unwrap(),
            PathStyle::Unix,
        )
        .unwrap();
        assert_eq!(
            script,
            "chmod 755 .zed_server/download-7-slirp4netns-linux-x86_64 \
             && mv .zed_server/download-7-slirp4netns-linux-x86_64 .zed_server/slirp4netns"
        );

        let script = install_uploaded_binary_script(
            RelPath::from_unix_str("my dir/download-7-x").unwrap(),
            RelPath::from_unix_str("my dir/x").unwrap(),
            PathStyle::Unix,
        )
        .unwrap();
        assert_eq!(
            script,
            "chmod 755 'my dir/download-7-x' && mv 'my dir/download-7-x' 'my dir/x'"
        );
    }
}
