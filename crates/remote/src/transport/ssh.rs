use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    command::{RemoteCommand, home_relative_path, home_stdio_launcher_command},
    remote_client::{CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions},
    transport::{
        HOME_DIR_ARGS, HOME_DIR_PROGRAM, find_bundled_server_artifact,
        legacy_server_binary_name_prefixes, parse_home_dir, parse_platform, parse_shell,
        server_binary_name_prefix,
    },
};
use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    AsyncReadExt as _, FutureExt as _,
    channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
    select_biased,
};
use gpui::{App, AppContext as _, AsyncApp, Task};
use parking_lot::Mutex;
use paths::remote_server_dir_relative;
use release_channel::{AppVersion, ReleaseChannel};
use rpc::proto::Envelope;
use semver::Version;
pub use settings_content::SshPortForwardOption;
use smol::fs;
use std::{
    fmt::Write as _,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use util::command::{Child, Stdio};
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
    shell::ShellKind,
};

/// How long to wait for SSH to connect when no askpass prompt has opened.
const SSH_CONNECTION_PROMPT_TIMEOUT: Duration = Duration::from_secs(17);

const BYTES_PER_MEGABYTE: u64 = 1024 * 1024;

/// Non-fatal problems noticed while establishing a connection, for the UI to
/// drain and show once the connection is up (see
/// [`take_remote_connection_warnings`]).
///
/// A process-wide queue rather than something on the connection because the
/// transport has no route to a workspace: `RemoteClientDelegate` can only set
/// the progress line of a modal that is about to close.
pub(super) static CONNECTION_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Takes everything a connection attempt wanted to tell the user but could not
/// fail over. Draining is deliberate: each warning is shown once.
pub fn take_remote_connection_warnings() -> Vec<String> {
    std::mem::take(&mut *CONNECTION_WARNINGS.lock())
}

/// The environment variables [`build_command_windows`] carries over to a
/// Windows remote, out of the whole environment it otherwise drops to stay
/// under Windows OpenSSH's 8K command-line limit.
///
/// The exception exists because the fork's network control depends on it: the
/// egress proxy the remote runs for AI child processes (ADR-F14) is reachable
/// only through these variables, so dropping them does not merely inconvenience
/// a Windows remote, it silently takes those processes *outside* the allowlist.
/// A proxy URL and a `NO_PROXY` list are a few dozen bytes, which the limit has
/// room for. Both cases are listed because the injectors write the uppercase
/// spelling while some tools only read the lowercase one, and the whole point
/// is to pass on whatever the caller decided to set.
const WINDOWS_FORWARDED_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// How many bytes of rendered `$env:…=…` assignments a Windows remote command
/// may spend out of the 8K it has. Small on purpose: the budget is there to let
/// a proxy configuration through, not to reopen the door the 8K limit closed.
/// The command is sent base64-encoded UTF-16, so a byte here costs about 2.7 on
/// the wire.
const WINDOWS_FORWARDED_ENV_MAX_BYTES: usize = 1024;

/// Separates the answers inside [`PosixProbe`]'s single command. Echoed on a
/// line of its own; nothing any of the probed commands prints contains it.
const PROBE_SECTION_SEPARATOR: &str = "--zed-probe--";

/// Everything the connection setup needs to know about a POSIX remote,
/// answered in a single `ssh` invocation.
///
/// Each of these used to be a separate `ssh`. On Windows that is expensive:
/// Win32-OpenSSH has no `ControlMaster`, so every invocation pays a fresh TCP
/// connect, key exchange and authentication — 250ms each against a LAN host on
/// 2026-09-04, and there were six of them before the proxy could even start.
/// The `ssh` that carries this probe is one of those six.
///
/// Diagnostics are unaffected: this is best effort in one piece, and anything
/// that does not parse (a Windows remote above all, whose shell cannot run the
/// script at all) falls back to the individual commands, which report their own
/// failures exactly as they did before.
struct PosixProbe {
    shell: String,
    platform: RemotePlatform,
    os_version: Option<String>,
    /// The server binary this probe asked about, so a caller that computes a
    /// different path for it knows not to use the two answers below.
    server_binary_path: String,
    /// Whether `<server_binary_path> version` ran. Deliberately *runs* the
    /// binary rather than testing for its presence: a copy that lost its execute
    /// bit, or sits on a `noexec` mount, has to be reinstalled and not reused.
    server_binary_runs: bool,
    /// `(size, mtime in seconds since the epoch)` of the server binary.
    server_binary_stat: Option<(u64, i64)>,
    slirp4netns_runs: bool,
    /// What `pwd` prints right after the `cd` every command starts with, for
    /// [`home_stdio_launcher_command`]. `None` when it did not parse, in which
    /// case the caller asks for it on its own.
    home_dir: Option<String>,
}

pub(crate) struct SshRemoteConnection {
    socket: SshSocket,
    master_process: Mutex<Option<MasterProcess>>,
    /// Whether `kill()` has been called. Separate from `master_process` because
    /// reused ControlMaster sessions start with `master_process` as `None`.
    killed: AtomicBool,
    remote_binary_path: Option<Arc<RelPath>>,
    ssh_platform: RemotePlatform,
    ssh_os_version: Option<String>,
    ssh_path_style: PathStyle,
    ssh_shell: String,
    ssh_shell_kind: ShellKind,
    ssh_default_system_shell: String,
    ssh_home_dir: Option<String>,
    _temp_dir: TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SshConnectionHost {
    IpAddr(IpAddr),
    Hostname(String),
}

impl SshConnectionHost {
    pub fn to_bracketed_string(&self) -> String {
        match self {
            Self::IpAddr(IpAddr::V4(ip)) => ip.to_string(),
            Self::IpAddr(IpAddr::V6(ip)) => format!("[{}]", ip),
            Self::Hostname(hostname) => hostname.clone(),
        }
    }

    pub fn to_string(&self) -> String {
        match self {
            Self::IpAddr(ip) => ip.to_string(),
            Self::Hostname(hostname) => hostname.clone(),
        }
    }
}

impl From<&str> for SshConnectionHost {
    fn from(value: &str) -> Self {
        if let Ok(address) = value.parse() {
            Self::IpAddr(address)
        } else {
            Self::Hostname(value.to_string())
        }
    }
}

impl From<String> for SshConnectionHost {
    fn from(value: String) -> Self {
        if let Ok(address) = value.parse() {
            Self::IpAddr(address)
        } else {
            Self::Hostname(value)
        }
    }
}

impl Default for SshConnectionHost {
    fn default() -> Self {
        Self::Hostname(Default::default())
    }
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

// Quote paths for sftp batch parsing and double backslashes for POSIX glob();
// Win32-OpenSSH accepts the same encoding.
fn escape_sftp_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len() + 2);
    escaped.push('"');
    for character in path.chars() {
        if character == '"' || character == '\\' {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('"');
    escaped
}

fn sftp_put_command(source_path: &str, destination_path: &str) -> String {
    let source = escape_sftp_path(source_path);
    let destination = escape_sftp_path(destination_path);
    format!("put {source} {destination}\n")
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SshConnectionOptions {
    pub host: SshConnectionHost,
    pub username: Option<String>,
    pub port: Option<u16>,
    pub password: Option<String>,
    pub args: Option<Vec<String>>,
    pub port_forwards: Option<Vec<SshPortForwardOption>>,
    pub connection_timeout: Option<u16>,

    pub nickname: Option<String>,
    pub upload_binary_over_ssh: bool,
}

impl From<settings_content::SshConnection> for SshConnectionOptions {
    fn from(val: settings_content::SshConnection) -> Self {
        SshConnectionOptions {
            host: val.host.to_string().into(),
            username: val.username,
            port: val.port,
            password: None,
            args: Some(val.args),
            nickname: val.nickname,
            upload_binary_over_ssh: val.upload_binary_over_ssh.unwrap_or_default(),
            port_forwards: val.port_forwards,
            connection_timeout: val.connection_timeout,
        }
    }
}

struct SshSocket {
    connection_options: SshConnectionOptions,
    #[cfg(not(windows))]
    socket_path: std::path::PathBuf,
    /// Extra environment variables needed for the ssh process
    envs: HashMap<String, String>,
    #[cfg(windows)]
    _proxy: askpass::PasswordProxy,
}

struct RemoteEnvironment {
    shell: String,
    platform: RemotePlatform,
    os_version: Option<String>,
}

impl RemoteEnvironment {
    fn posix_script() -> String {
        r#"
            platform=$(uname -sm) || exit;
            printf "\000%s\000%s\000" "$SHELL" "$platform";
            status=0;
            case "$platform" in
                "Linux "*) cat /etc/os-release || status=$?;;
                "Darwin "*) sw_vers -productVersion || status=$?;;
            esac;
            printf "\000%s" "$status";
        "#
        // Remove extra whitespaces
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
    }

    fn windows_script() -> String {
        r#"
            $shell = "";
            try {
                $process = Get-CimInstance Win32_Process -Filter "ProcessId=$PID" -ErrorAction Stop;
                $shell = (Get-CimInstance Win32_Process -Filter "ProcessId=$($process.ParentProcessId)" -ErrorAction Stop).Name;
            } catch {
                Write-Error -ErrorRecord $_ -ErrorAction Continue;
            };

            $architecture = $env:PROCESSOR_ARCHITEW6432;
            if (-not $architecture) {
                $architecture = $env:PROCESSOR_ARCHITECTURE;
            };

            $version = "";
            $status = 1;
            try {
                $version = cmd.exe /c ver;
                $status = $LASTEXITCODE;
            } catch {
                Write-Error -ErrorRecord $_ -ErrorAction Continue;
            };

            Write-Output ([string]::Join([char]0, @("", $shell, ($architecture -join "`n"), ($version -join "`n"), $status)));
        "#
        // Remove extra whitespaces
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
    }

    fn parse_posix(output: &str) -> Result<Self> {
        let mut fields = output.splitn(5, '\0').skip(1);

        let shell = parse_shell(fields.next().context("Missing remote shell")?, "sh");
        let platform = parse_platform(fields.next().context("Missing remote platform")?)?;
        let version = fields.next().context("Missing remote OS version")?;
        let status = fields
            .next()
            .context("Missing remote OS version exit status")?;
        let os_version = if status == "0" {
            super::parse_os_version(platform.os, version)
        } else {
            log::warn!("Failed to determine remote OS version: exit status {status}");
            None
        };

        Ok(Self {
            shell,
            platform,
            os_version,
        })
    }

    fn parse_windows(output: &str) -> Result<Self> {
        let mut fields = output.splitn(5, '\0').skip(1);

        let shell = parse_shell(fields.next().context("Missing remote shell")?, "cmd.exe");
        let architecture = fields
            .next()
            .context("Missing remote Windows architecture")?
            .trim();
        let platform = RemotePlatform {
            os: RemoteOs::Windows,
            arch: match architecture {
                "AMD64" => RemoteArch::X86_64,
                "ARM64" => RemoteArch::Aarch64,
                architecture => anyhow::bail!(
                    "Prebuilt remote servers are not yet available for windows-{architecture}. See https://zed.dev/docs/remote-development"
                ),
            },
        };
        let version = fields.next().context("Missing remote OS version")?;
        let status = fields
            .next()
            .context("Missing remote OS version exit status")?
            .trim();
        let os_version = if status == "0" {
            super::parse_os_version(RemoteOs::Windows, version)
        } else {
            log::warn!("Failed to determine remote OS version: exit status {status}");
            None
        };

        Ok(Self {
            shell,
            platform,
            os_version,
        })
    }
}

struct MasterProcess {
    process: Child,
}

#[cfg(not(windows))]
impl MasterProcess {
    pub fn new(
        askpass_script_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        socket_path: &std::path::Path,
        destination: &str,
    ) -> Result<Self> {
        let args = [
            "-N",
            "-o",
            "ControlPersist=no",
            "-o",
            "ControlMaster=yes",
            "-o",
        ];

        let mut master_process = util::command::new_command("ssh");
        master_process
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", askpass_script_path)
            .args(additional_args)
            .args(args);

        master_process.arg(format!("ControlPath={}", socket_path.display()));

        let process = master_process.arg(&destination).spawn()?;

        Ok(MasterProcess { process })
    }

    pub async fn wait_connected(&mut self) -> Result<()> {
        let Some(mut stdout) = self.process.stdout.take() else {
            anyhow::bail!("ssh process stdout capture failed");
        };

        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        Ok(())
    }
}

#[cfg(windows)]
impl MasterProcess {
    const CONNECTION_ESTABLISHED_MAGIC: &str = "ZED_SSH_CONNECTION_ESTABLISHED";

    pub fn new(
        askpass_script_path: &std::ffi::OsStr,
        askpass_socket_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        destination: &str,
    ) -> Result<Self> {
        // On Windows, `ControlMaster` and `ControlPath` are not supported:
        // https://github.com/PowerShell/Win32-OpenSSH/issues/405
        // https://github.com/PowerShell/Win32-OpenSSH/wiki/Project-Scope
        //
        // Using an ugly workaround to detect connection establishment
        // -N doesn't work with JumpHosts as windows openssh never closes stdin in that case
        let args = [
            "-t",
            &format!("echo '{}'; exec $0", Self::CONNECTION_ESTABLISHED_MAGIC),
        ];

        let mut master_process = util::command::new_command("ssh");
        master_process
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", askpass_script_path)
            .env("ZED_ASKPASS_SOCKET", askpass_socket_path)
            .args(additional_args)
            .arg(destination)
            .args(args);

        let process = master_process.spawn()?;

        Ok(MasterProcess { process })
    }

    pub async fn wait_connected(&mut self) -> Result<()> {
        use smol::io::AsyncBufReadExt;

        let Some(stdout) = self.process.stdout.take() else {
            anyhow::bail!("ssh process stdout capture failed");
        };

        let mut reader = smol::io::BufReader::new(stdout);

        let mut line = String::new();

        loop {
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                anyhow::bail!("ssh process exited before connection established");
            }

            if line.contains(Self::CONNECTION_ESTABLISHED_MAGIC) {
                return Ok(());
            }
        }
    }
}

impl AsRef<Child> for MasterProcess {
    fn as_ref(&self) -> &Child {
        &self.process
    }
}

impl AsMut<Child> for MasterProcess {
    fn as_mut(&mut self) -> &mut Child {
        &mut self.process
    }
}

#[async_trait(?Send)]
impl RemoteConnection for SshRemoteConnection {
    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::Release);
        let Some(mut process) = self.master_process.lock().take() else {
            log::debug!("no master process to kill (external ControlMaster session)");
            return Ok(());
        };
        process.as_mut().kill().ok();
        process.as_mut().status().await?;
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Ssh(self.socket.connection_options.clone())
    }

    fn shell(&self) -> String {
        self.ssh_shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.ssh_default_system_shell.clone()
    }

    fn build_command(
        &self,
        input_program: Option<String>,
        input_args: &[String],
        input_env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let Self {
            ssh_path_style,
            socket,
            ssh_shell_kind,
            ssh_shell,
            ..
        } = self;
        let env = socket.envs.clone();

        if self.ssh_platform.os.is_windows() {
            build_command_windows(
                input_program,
                input_args,
                input_env,
                working_dir,
                port_forward,
                env,
                *ssh_path_style,
                ssh_shell,
                *ssh_shell_kind,
                socket.ssh_command_options(),
                &socket.connection_options.ssh_destination(),
                interactive,
            )
        } else {
            build_command_posix(
                input_program,
                input_args,
                input_env,
                working_dir,
                port_forward,
                env,
                *ssh_path_style,
                ssh_shell,
                *ssh_shell_kind,
                socket.ssh_command_options(),
                &socket.connection_options.ssh_destination(),
                interactive,
            )
        }
    }

    fn build_stdio_command(
        &self,
        mut command: RemoteCommand,
    ) -> Result<(CommandTemplate, Vec<u8>)> {
        if self.ssh_platform.os.is_windows() {
            // A Windows remote has no stdin launcher, so the environment goes
            // through `build_command_windows`, which drops all of it except
            // the egress proxy (ADR-F14): dropping that too would run the
            // process outside the allowlist without a word.
            let template = self.build_command(
                Some(command.program),
                &command.args,
                &command.env,
                command.working_dir,
                None,
                Interactive::No,
            )?;
            return Ok((template, Vec::new()));
        }
        let remote_binary_path = self
            .remote_binary_path
            .as_ref()
            .context("Remote binary path not set")?;
        command.retain_valid_env();
        let mut exec = posix_working_dir_prefix(
            command.working_dir.take(),
            self.ssh_path_style,
            self.ssh_shell_kind,
        )?;
        exec.push_str(&home_stdio_launcher_command(
            self.ssh_shell_kind,
            self.ssh_home_dir.as_deref(),
            &remote_binary_path.display(PathStyle::Unix),
        )?);
        let template = ssh_command_template(
            self.socket.ssh_command_options(),
            None,
            Interactive::No,
            &self.socket.connection_options.ssh_destination(),
            exec,
            self.socket.envs.clone(),
        );
        Ok((template, command.encode()?))
    }

    fn build_forward_ports_command(
        &self,
        forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        let Self { socket, .. } = self;
        let mut args = socket.ssh_command_options();
        args.push("-N".into());
        for (local_port, host, remote_port) in forwards {
            args.push("-L".into());
            args.push(format!(
                "{}:{}:{}",
                local_port,
                bracket_ipv6(&host),
                remote_port
            ));
        }
        args.push(socket.connection_options.ssh_destination());
        Ok(CommandTemplate {
            program: "ssh".into(),
            args,
            env: Default::default(),
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

        let mut sftp_command = self.build_sftp_command();
        sftp_command.kill_on_drop(true);
        let mut scp_command =
            self.build_scp_command(&src_path, &dest_path_str, Some(&["-C", "-r"]));
        scp_command.kill_on_drop(true);

        cx.background_spawn(async move {
            // We will try SFTP first, and if that fails, we will fall back to SCP.
            // If SCP fails also, we give up and return an error.
            // The reason we allow a fallback from SFTP to SCP is that if the user has to specify a password,
            // depending on the implementation of SSH stack, SFTP may disable interactive password prompts in batch mode.
            // This is for example the case on Windows as evidenced by this implementation snippet:
            // https://github.com/PowerShell/openssh-portable/blob/b8c08ef9da9450a94a9c5ef717d96a7bd83f3332/sshconnect2.c#L417
            let mut sftp_failure = None;
            if Self::is_sftp_available().await {
                log::debug!("using SFTP for directory upload");
                let mut child = sftp_command.spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    use futures::AsyncWriteExt;
                    let sftp_batch = format!("put -r \"{src_path_display}\" \"{dest_path_str}\"\n");
                    stdin.write_all(sftp_batch.as_bytes()).await?;
                    stdin.flush().await?;
                }

                let output = child.output().await?;
                if output.status.success() {
                    return Ok(());
                }

                let stderr = String::from_utf8_lossy(&output.stderr);
                log::debug!("failed to upload directory via SFTP {src_path_display} -> {dest_path_str}: {stderr}");
                sftp_failure = Some(stderr.trim().to_string());
            }

            log::debug!("using SCP for directory upload");
            let output = scp_command.output().await?;

            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("failed to upload directory via SCP {src_path_display} -> {dest_path_str}: {stderr}");

            // Both `No space left on device` and `Permission denied` surface
            // here, so the wording has to be one the user can act on rather
            // than a transport name and a typo.
            anyhow::bail!(i18n::t!(
                "Failed to upload the directory to the remote ({src_path_display} -> {dest_path_str}): \
                 SCP: {scp}{sftp}",
                scp = stderr.trim(),
                sftp = sftp_failure
                    .map(|reason| format!(" / SFTP: {reason}"))
                    .unwrap_or_default(),
            ));
        })
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
        const VARS: [&str; 3] = ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS"];
        delegate.set_status(Some(i18n::t!("Starting proxy")), cx);

        let Some(remote_binary_path) = self.remote_binary_path.clone() else {
            return Task::ready(Err(anyhow!(i18n::t!("Remote binary path not set"))));
        };

        let mut ssh_command = if self.ssh_platform.os.is_windows() {
            // TODO: Set the `VARS` environment variables, we do not have `env` on windows
            // so this needs a different approach
            let mut proxy_args = vec![];
            proxy_args.push("proxy".to_owned());
            proxy_args.push("--identifier".to_owned());
            proxy_args.push(unique_identifier);

            if reconnect {
                proxy_args.push("--reconnect".to_owned());
            }
            self.socket.ssh_command(
                self.ssh_shell_kind,
                &remote_binary_path.display(self.path_style()),
                &proxy_args,
                false,
            )
        } else {
            let mut proxy_args = vec![];
            for env_var in VARS {
                if let Some(value) = std::env::var(env_var).ok() {
                    proxy_args.push(format!("{env_var}={value}"));
                }
            }
            proxy_args.push(remote_binary_path.display(self.path_style()).into_owned());
            proxy_args.push("proxy".to_owned());
            proxy_args.push("--identifier".to_owned());
            proxy_args.push(unique_identifier);

            if reconnect {
                proxy_args.push("--reconnect".to_owned());
            }
            self.socket
                .ssh_command(self.ssh_shell_kind, "env", &proxy_args, false)
        };

        let ssh_proxy_process = match ssh_command
            // IMPORTANT: we kill this process when we drop the task that uses it.
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
            ssh_proxy_process,
            incoming_tx,
            outgoing_rx,
            connection_activity_tx,
            cx,
        )
    }

    fn path_style(&self) -> PathStyle {
        self.ssh_path_style
    }

    fn remote_platform(&self) -> RemotePlatform {
        self.ssh_platform
    }

    fn remote_server_binary_path(&self) -> Option<Arc<RelPath>> {
        self.remote_binary_path.clone()
    }

    fn remote_os_version(&self) -> Option<String> {
        self.ssh_os_version.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}

/// Check if the user already has an active SSH ControlMaster session for the
/// given destination. See: https://github.com/zed-industries/zed/issues/45271
#[cfg(not(windows))]
async fn find_existing_control_master(
    destination: &str,
    additional_args: &[String],
) -> Option<PathBuf> {
    // Use `ssh -G` to resolve the user's effective SSH config for this host.
    // This expands ControlPath tokens (%h, %p, %r, %C, etc.) into actual paths.
    let output = match util::command::new_command("ssh")
        .args(additional_args)
        .arg("-G")
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("failed to run ssh -G: {e}");
            return None;
        }
    };

    if !output.status.success() {
        log::debug!("ssh -G failed for {destination}, skipping ControlMaster reuse");
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let control_path = stdout.lines().find_map(|line| {
        let path = line.strip_prefix("controlpath ")?.trim();
        if path == "none" || path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    })?;

    // Verify the master is actually alive by sending a control command.
    let check = match util::command::new_command("ssh")
        .args(additional_args)
        .args(["-O", "check"])
        .arg("-o")
        .arg(format!("ControlPath={}", control_path.display()))
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("failed to run ssh -O check: {e}");
            return None;
        }
    };

    if check.status.success() {
        log::info!(
            "reusing existing SSH ControlMaster at {}",
            control_path.display()
        );
        Some(control_path)
    } else {
        log::debug!(
            "ControlMaster socket at {} is not alive, creating new connection",
            control_path.display()
        );
        None
    }
}

impl SshRemoteConnection {
    pub(crate) async fn new(
        connection_options: SshConnectionOptions,
        known_os: Option<RemoteOs>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        use askpass::AskPassResult;

        let connect_started = Instant::now();
        let destination = connection_options.ssh_destination();

        let temp_dir = tempfile::Builder::new()
            .prefix("zed-ssh-session")
            .tempdir()?;

        // On non-Windows, check if the user already has an active ControlMaster
        // session for this host. If so, reuse it instead of prompting for auth.
        #[cfg(not(windows))]
        let reused_socket =
            find_existing_control_master(&destination, &connection_options.additional_args()).await;

        #[cfg(not(windows))]
        let (socket, master_process_option) = if let Some(reused_path) = reused_socket {
            delegate.set_status(Some(i18n::t!("Connecting (reusing session)")), cx);
            log::info!("reusing existing ControlMaster, skipping authentication");
            let socket = SshSocket::new(connection_options, reused_path).await?;
            (socket, None)
        } else {
            let askpass_delegate = askpass::AskPassDelegate::new_with_cancellation(cx, {
                let delegate = delegate.clone();
                move |prompt, tx, cancellation, cx| {
                    delegate.ask_password(prompt, tx, cancellation, cx)
                }
            });

            let mut askpass =
                askpass::AskPassSession::new(cx.background_executor().clone(), askpass_delegate)
                    .await?;

            delegate.set_status(Some(i18n::t!("Connecting")), cx);

            // Start the master SSH process, which does not do anything except
            // for establish the connection and keep it open, allowing other ssh
            // commands to reuse it via a control socket.
            let socket_path = temp_dir.path().join("ssh.sock");
            let mut master_process = MasterProcess::new(
                askpass.script_path().as_ref(),
                connection_options.additional_args(),
                &socket_path,
                &destination,
            )?;

            let result = select_biased! {
                result = askpass.run(Some(SSH_CONNECTION_PROMPT_TIMEOUT)).fuse() => {
                    match result {
                        AskPassResult::CancelledByUser => {
                            master_process.as_mut().kill().ok();
                            anyhow::bail!(i18n::t!("SSH connection canceled"))
                        }
                        AskPassResult::Timedout => {
                            anyhow::bail!(i18n::t!("connecting to host timed out"))
                        }
                    }
                }
                _ = master_process.wait_connected().fuse() => {
                    anyhow::Ok(())
                }
            };

            if let Err(e) = result {
                return Err(e.context(i18n::t!("Failed to connect to host")));
            }

            if master_process.as_mut().try_status()?.is_some() {
                let mut output = Vec::new();
                let mut stderr = master_process.as_mut().stderr.take().unwrap();
                stderr.read_to_end(&mut output).await?;

                let error_message = i18n::t!(
                    "failed to connect: {}",
                    String::from_utf8_lossy(&output).trim()
                );
                anyhow::bail!(error_message);
            }

            let socket = SshSocket::new(connection_options, socket_path).await?;
            drop(askpass);
            (socket, Some(master_process))
        };

        #[cfg(windows)]
        let (socket, master_process_option) = {
            let askpass_delegate = askpass::AskPassDelegate::new_with_cancellation(cx, {
                let delegate = delegate.clone();
                move |prompt, tx, cancellation, cx| {
                    delegate.ask_password(prompt, tx, cancellation, cx)
                }
            });

            let mut askpass =
                askpass::AskPassSession::new(cx.background_executor().clone(), askpass_delegate)
                    .await?;

            delegate.set_status(Some(i18n::t!("Connecting")), cx);

            let mut master_process = MasterProcess::new(
                askpass.script_path().as_ref(),
                askpass.socket_path().as_ref(),
                connection_options.additional_args(),
                &destination,
            )?;

            let result = select_biased! {
                result = askpass.run(Some(SSH_CONNECTION_PROMPT_TIMEOUT)).fuse() => {
                    match result {
                        AskPassResult::CancelledByUser => {
                            master_process.as_mut().kill().ok();
                            anyhow::bail!(i18n::t!("SSH connection canceled"))
                        }
                        AskPassResult::Timedout => {
                            anyhow::bail!(i18n::t!("connecting to host timed out"))
                        }
                    }
                }
                _ = master_process.wait_connected().fuse() => {
                    anyhow::Ok(())
                }
            };

            if let Err(e) = result {
                return Err(e.context(i18n::t!("Failed to connect to host")));
            }

            // Without ControlMaster the master process only exists to drive
            // askpass; once the MAGIC line arrived it may already have exited
            // *successfully* (no pty was allocated, so `exec $0` hits EOF right
            // away). Only a non-zero exit is a connection failure — treating a
            // clean exit as one made connections fail at random, with ssh's
            // harmless "Pseudo-terminal will not be allocated" note as the
            // reported reason.
            if let Some(status) = master_process.as_mut().try_status()?
                && !status.success()
            {
                let mut output = Vec::new();
                let mut stderr = master_process.as_mut().stderr.take().unwrap();
                stderr.read_to_end(&mut output).await?;

                let error_message = i18n::t!(
                    "failed to connect: {}",
                    String::from_utf8_lossy(&output).trim()
                );
                anyhow::bail!(error_message);
            }

            let socket = SshSocket::new(
                connection_options,
                askpass
                    .get_password()
                    .or_else(|| askpass::EncryptedPassword::try_from("").ok())
                    .context("Failed to fetch askpass password")?,
                cx.background_executor().clone(),
            )
            .await?;
            drop(askpass);

            (socket, Some(master_process))
        };

        log::info!(
            "remote connect timing: ssh transport ready in {:?}",
            connect_started.elapsed()
        );

        let (release_channel, version) =
            cx.update(|cx| (ReleaseChannel::global(cx), AppVersion::global(cx)));

        delegate.set_status(Some(i18n::t!("Checking the remote environment")), cx);
        let probe_started = Instant::now();
        // Tried before anything else because on a POSIX remote it answers, in
        // one round trip, what the sequence below needs four for — plus the two
        // that `ensure_server_binary` and `ensure_slirp4netns_binary` would each
        // spend on their own. A Windows remote cannot run it and falls through
        // to that sequence, paying one wasted round trip, which a reconnect that
        // already knows the remote runs Windows skips.
        let posix_probe = if known_os.is_some_and(|os| os.is_windows()) {
            None
        } else {
            let (_, binary_name) = server_binary_names(release_channel, &version, false);
            let server_binary_path = remote_server_dir_relative()
                .join(
                    RelPath::from_unix_str(&binary_name)
                        .context("building the remote server binary path")?,
                )
                .display(PathStyle::Unix)
                .into_owned();
            let slirp4netns_path = remote_server_dir_relative()
                .join(
                    RelPath::from_unix_str("slirp4netns")
                        .context("building the remote slirp4netns path")?,
                )
                .display(PathStyle::Unix)
                .into_owned();
            socket
                .probe_posix(&server_binary_path, &slirp4netns_path)
                .await
        };

        let (is_windows, ssh_shell, ssh_platform, ssh_os_version) = match &posix_probe {
            Some(probe) => (
                false,
                probe.shell.clone(),
                probe.platform,
                probe.os_version.clone(),
            ),
            None => {
                let is_windows = match known_os {
                    Some(os) => os.is_windows(),
                    None => socket.probe_is_windows().await,
                };
                let RemoteEnvironment {
                    shell,
                    platform,
                    os_version,
                } = if is_windows {
                    socket.environment_windows().await?
                } else {
                    socket.environment_posix().await?
                };
                (is_windows, shell, platform, os_version)
            }
        };
        log::info!("Remote is windows: {}", is_windows);
        log::info!("Remote shell discovered: {}", ssh_shell);
        log::info!("Remote platform discovered: {:?}", ssh_platform);
        log::info!("Remote OS version discovered: {:?}", ssh_os_version);
        log::info!(
            "remote connect timing: remote environment probed in {:?} ({}){}",
            probe_started.elapsed(),
            if posix_probe.is_some() {
                "1 ssh round trip"
            } else {
                "separate ssh round trips, the batched probe did not apply"
            },
            match posix_probe
                .as_ref()
                .and_then(|probe| probe.server_binary_stat)
            {
                Some((size, _)) => format!(
                    "; the remote already holds a {} server binary",
                    format_transfer_size(size)
                ),
                None => String::new(),
            }
        );

        let ssh_shell_kind = ShellKind::new(&ssh_shell, is_windows);
        let ssh_home_dir = if is_windows {
            None
        } else if let Some(home_dir) = posix_probe
            .as_ref()
            .and_then(|probe| probe.home_dir.clone())
        {
            Some(home_dir)
        } else {
            socket.home_dir(ssh_shell_kind).await
        };
        log::info!("Remote home directory discovered: {:?}", ssh_home_dir);

        let (ssh_path_style, ssh_default_system_shell) = match ssh_platform.os {
            RemoteOs::Windows => (PathStyle::Windows, ssh_shell.clone()),
            _ => (PathStyle::Unix, String::from("/bin/sh")),
        };

        let mut this = Self {
            socket,
            master_process: Mutex::new(master_process_option),
            killed: AtomicBool::new(false),
            _temp_dir: temp_dir,
            remote_binary_path: None,
            ssh_path_style,
            ssh_platform,
            ssh_os_version,
            ssh_shell,
            ssh_shell_kind,
            ssh_default_system_shell,
            ssh_home_dir,
        };

        let server_binary_started = Instant::now();
        this.remote_binary_path = Some(
            this.ensure_server_binary(
                posix_probe.as_ref(),
                &delegate,
                release_channel,
                version,
                cx,
            )
            .await?,
        );
        log::info!(
            "remote connect timing: remote server binary ensured in {:?}",
            server_binary_started.elapsed()
        );

        // ADR-F18: install the bundled `slirp4netns` next to the remote server
        // binary so the on-host sandbox helper can find it. Best-effort: a
        // missing slirp only disables remote agent sandboxing (the helper fails
        // closed at run time); it must not block the connection or human
        // terminals.
        if let Err(error) = this
            .ensure_slirp4netns_binary(posix_probe.as_ref(), &delegate, cx)
            .await
        {
            super::warn_slirp4netns_not_installed(&error);
        }

        log::info!(
            "remote connect timing: ssh connection established in {:?}",
            connect_started.elapsed()
        );
        Ok(this)
    }

    /// ADR-F18: ensure `~/.zed_server/slirp4netns` exists on a Linux remote,
    /// uploading the bundled binary if it is absent. Idempotent (skips when a
    /// working `slirp4netns` is already present) and a no-op on non-Linux
    /// remotes.
    async fn ensure_slirp4netns_binary(
        &self,
        probe: Option<&PosixProbe>,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if self.ssh_platform.os != RemoteOs::Linux {
            return Ok(());
        }
        let dst_path = paths::remote_server_dir_relative()
            .join(RelPath::from_unix_str("slirp4netns").context("building slirp4netns dst path")?);
        let already_present = match probe {
            Some(probe) => probe.slirp4netns_runs,
            None => self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    &dst_path.display(self.path_style()),
                    &["--version"],
                    true,
                )
                .await
                .is_ok(),
        };
        if already_present {
            return Ok(());
        }
        let Some(bundled_path) = super::find_bundled_slirp4netns(self.ssh_platform) else {
            anyhow::bail!("no slirp4netns bundled next to this executable");
        };
        log::info!(
            "installing bundled slirp4netns for the remote sandbox: {}",
            bundled_path.display()
        );
        self.upload_and_install_server_binary(&bundled_path, &dst_path, delegate, cx)
            .await
            .context("installing bundled slirp4netns")
    }

    async fn ensure_server_binary(
        &self,
        probe: Option<&PosixProbe>,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: Version,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        // The prefix is kept because the cleanup pass below globs on it: it is
        // the part of the name that is stable across the versions of this
        // channel that have piled up on the remote.
        let (binary_name_prefix, binary_name) =
            server_binary_names(release_channel, &version, self.ssh_platform.os.is_windows());
        let legacy_binary_name_prefixes = legacy_server_binary_name_prefixes(release_channel);
        let dst_path = paths::remote_server_dir_relative().join(
            RelPath::from_unix_str(&binary_name)
                .context("building the remote server binary path")?,
        );
        let dst_path_display = dst_path.display(self.path_style()).into_owned();

        let binary_exists_on_server = match probe {
            Some(probe) if probe.server_binary_path == dst_path_display => probe.server_binary_runs,
            _ => self
                .socket
                .run_command(self.ssh_shell_kind, &dst_path_display, &["version"], true)
                .await
                .is_ok(),
        };

        // Recorded regardless of outcome (built, skipped, or failed) so that,
        // if every other fallback below also comes up empty, the final error
        // can explain *why* no source build bailed us out — cmake/zig
        // missing from $PATH is the recurring cause (TODO F11 ⑬).
        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        let source_build_status = match super::build_remote_server_from_source(
            &self.ssh_platform,
            delegate.as_ref(),
            binary_exists_on_server,
            cx,
        )
        .await
        {
            Ok(Some(remote_server_path)) => {
                log::info!(
                    "remote server binary source: locally built {}",
                    remote_server_path.display()
                );
                // The cargo build above is incremental, so on the overwhelming
                // majority of connections it rebuilds nothing and hands back the
                // very binary the remote is already running. Uploading it again
                // (~90MB), unpacking it and sweeping the directory is then pure
                // waste, and it is what every single connection used to pay.
                if binary_exists_on_server
                    && self
                        .locally_built_binary_is_already_installed(
                            &remote_server_path,
                            &dst_path,
                            probe.and_then(|probe| {
                                (probe.server_binary_path == dst_path_display)
                                    .then_some(probe.server_binary_stat)
                                    .flatten()
                            }),
                            delegate,
                            cx,
                        )
                        .await
                {
                    return Ok(dst_path.into());
                }
                self.remove_stale_server_files(
                    &binary_name_prefix,
                    &legacy_binary_name_prefixes,
                    &binary_name,
                )
                .await;
                self.upload_and_install_server_binary(&remote_server_path, &dst_path, delegate, cx)
                    .await
                    .context("installing locally built server binary")?;
                return Ok(dst_path.into());
            }
            Ok(None) => {
                let build_remote_server = std::env::var("ZED_BUILD_REMOTE_SERVER")
                    .unwrap_or_else(|_| i18n::t!("nocompress (default)").to_string());
                i18n::t!("not run (ZED_BUILD_REMOTE_SERVER={build_remote_server})")
            }
            // A missing cross-compilation toolchain (zig, cargo-zigbuild, cmake) must not be
            // fatal here: the bundled artifact below is exactly the fallback for that case.
            Err(error) => {
                log::warn!(
                    "failed to build the remote server from source, \
                     falling back to a bundled or downloaded binary: {error:#}"
                );
                format!("{error:#}")
            }
        };
        #[cfg(not(any(debug_assertions, feature = "build-remote-server-binary")))]
        let source_build_status =
            i18n::t!("this build cannot build the remote server from source").to_string();

        if binary_exists_on_server {
            log::info!(
                "remote server binary source: already present on the remote at {}",
                dst_path.display(self.path_style())
            );
            return Ok(dst_path.into());
        }

        if let Some(bundled_path) = find_bundled_server_artifact(self.ssh_platform) {
            log::info!(
                "remote server binary source: artifact bundled with this app, {}",
                bundled_path.display()
            );
            self.remove_stale_server_files(
                &binary_name_prefix,
                &legacy_binary_name_prefixes,
                &binary_name,
            )
            .await;
            self.upload_and_install_server_binary(&bundled_path, &dst_path, delegate, cx)
                .await
                .context("installing bundled server binary")?;
            return Ok(dst_path.into());
        }

        log::info!("remote server binary source: download (no local or bundled binary available)");

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => {
                let bundled_artifact_name = super::bundled_server_artifact_names(self.ssh_platform)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let bundled_search_dirs = super::bundled_server_search_dirs()
                    .map(|dirs| {
                        dirs.iter()
                            .map(|dir| dir.display().to_string())
                            .collect::<Vec<_>>()
                            .join(" / ")
                    })
                    .unwrap_or_else(|| {
                        i18n::t!("not searched, because the executable's location is unknown")
                            .to_string()
                    });
                anyhow::bail!(i18n::t!(
                    "No remote server binary is available. \
                     Not on the remote at {remote_path} / \
                     bundled artifact `{artifact}` not found in {search_dirs} / \
                     build from source: {source_build_status}. \
                     The automatic build fails when cmake or zig is not on PATH. \
                     Building with script\\build.ps1 prepares the bundled artifact.",
                    remote_path = dst_path.display(self.path_style()),
                    artifact = bundled_artifact_name,
                    search_dirs = bundled_search_dirs,
                ))
            }
            _ => Ok(Some(AppVersion::global(cx))),
        })?;

        let tmp_path_compressed = remote_server_dir_relative().join(
            RelPath::from_unix_str(&format!(
                "{}-download-{}.{}",
                binary_name,
                std::process::id(),
                if self.ssh_platform.os.is_windows() {
                    "zip"
                } else {
                    "gz"
                }
            ))
            .unwrap(),
        );
        self.remove_stale_server_files(
            &binary_name_prefix,
            &legacy_binary_name_prefixes,
            &binary_name,
        )
        .await;

        if !self.socket.connection_options.upload_binary_over_ssh
            && let Some(url) = delegate
                .get_download_url(
                    self.ssh_platform,
                    release_channel,
                    wanted_version.clone(),
                    cx,
                )
                .await?
        {
            match self
                .download_binary_on_server(&url, &tmp_path_compressed, delegate, cx)
                .await
            {
                Ok(_) => {
                    self.extract_server_binary(&dst_path, &tmp_path_compressed, delegate, cx)
                        .await
                        .context(i18n::t!("extracting server binary"))?;
                    return Ok(dst_path.into());
                }
                Err(e) => {
                    log::error!(
                        "Failed to download binary on server, attempting to download locally and then upload it the server: {e:#}",
                    )
                }
            }
        }

        // Reached only once both the remote-side download and the two
        // earlier local fallbacks (already present, bundled next to the
        // exe) have all come up empty, so a failure here means this
        // connection has no server binary at all. This fork blocks egress
        // by default, so an allowlist rejection is the expected failure
        // mode on a non-`Dev` channel — the same guidance the `Dev`-channel
        // error above gives is repeated here for that reason.
        let src_path = delegate
            .download_server_binary_locally(
                self.ssh_platform,
                release_channel,
                wanted_version.clone(),
                cx,
            )
            .await
            .context(i18n::t!("downloading server binary locally"))
            .context(i18n::t!(
                "There is no bundled artifact and the download failed as well. \
                 Build with script\\build.ps1 to prepare the bundled artifact, \
                 or check that this computer can reach the download server"
            ))?;
        self.upload_local_server_binary(&src_path, &tmp_path_compressed, delegate, cx)
            .await
            .context(i18n::t!("uploading server binary"))?;
        self.extract_server_binary(&dst_path, &tmp_path_compressed, delegate, cx)
            .await
            .context(i18n::t!("extracting server binary"))?;
        Ok(dst_path.into())
    }

    /// Whether the binary just built locally is byte-for-byte the one already
    /// installed at `dst_path`, in which case the upload, the unpack and the
    /// stale-file sweep can all be skipped.
    ///
    /// Only ever consulted when `<dst_path> version` already ran successfully on
    /// the remote: "the bytes match" alone would also be true of a binary that
    /// is present but *not runnable* there (lost its execute bit, sits on a
    /// `noexec` mount), and skipping the install in that case would turn a
    /// recoverable state into a failed connection. The bundled-artifact path
    /// below is not deduplicated for exactly that reason — it is only ever
    /// reached when the remote's copy did *not* run.
    ///
    /// Best effort in both directions: anything that goes wrong here (no hashing
    /// tool on the remote, an unreadable local artifact, output in a shape this
    /// does not recognize) falls back to uploading, which is what always used to
    /// happen.
    ///
    /// The two digests are only computed when the answer cannot be recalled: a
    /// verdict is remembered per remote and binary name, keyed on the size and
    /// modification time of *both* copies, so a connection where neither has
    /// been touched since the last one costs nothing at all. Reading ~600MB
    /// twice was measured at 10 of the ~15 seconds of a warm connection on
    /// 2026-09-04.
    #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
    async fn locally_built_binary_is_already_installed(
        &self,
        src_path: &Path,
        dst_path: &RelPath,
        remote_stat: Option<(u64, i64)>,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> bool {
        // What sits at `dst_path` is the *unpacked* binary, so a compressed
        // local artifact would have to be decompressed to be comparable. That
        // only arises when `ZED_BUILD_REMOTE_SERVER` asks for compression (the
        // default is `nocompress`), and compressing costs more per connection
        // than the upload this would save, so the compressed case just uploads.
        if let Some(extension) = src_path
            .extension()
            .and_then(|extension| extension.to_str())
            && matches!(extension, "gz" | "zip")
        {
            log::info!(
                "not comparing the locally built server binary with the remote's copy: \
                 {} is compressed, and the remote holds the unpacked binary",
                src_path.display()
            );
            return false;
        }

        // `connection_string` rather than `ssh_destination` because it carries
        // the port: two remotes reachable under the same name differ only there.
        let install_key = format!(
            "{}|{}",
            self.socket.connection_options.connection_string(),
            dst_path.display(self.path_style())
        );
        let local_stat = super::local_file_stat(src_path).await;

        if let Some((local_size, local_mtime_nanos)) = local_stat
            && let Some((remote_size, remote_mtime_secs)) = remote_stat
            && let Some(recorded) = super::read_remote_server_install(&install_key).await
            && recorded.local_size == local_size
            && recorded.local_mtime_nanos == local_mtime_nanos
            && recorded.remote_size == remote_size
            && recorded.remote_mtime_secs == remote_mtime_secs
        {
            log::info!(
                "remote server binary source: locally built, identical to the copy already on \
                 the remote (sha256 {}) — skipping upload, and skipping the comparison itself \
                 because neither copy has changed since the last connection",
                recorded.sha256
            );
            return true;
        }

        // Without this the "building" status would stay up for the hashing too,
        // which for an incremental no-op build is the longer of the two steps.
        delegate.set_status(Some(i18n::t!("Verifying remote development server")), cx);

        let local_digest = {
            let src_path = src_path.to_path_buf();
            // Hashing ~600MB is enough CPU and disk work to be worth keeping off
            // the thread that draws the connection dialog.
            match cx
                .background_spawn(async move { super::sha256_of_file(&src_path).await })
                .await
            {
                Ok(digest) => digest,
                Err(error) => {
                    log::warn!("could not hash the locally built server binary: {error:#}");
                    return false;
                }
            }
        };

        let Some(remote_digest) = self.remote_file_sha256(dst_path).await else {
            return false;
        };

        if local_digest != remote_digest {
            log::info!(
                "the server binary on the remote differs from the one just built \
                 (remote sha256 {remote_digest}, local sha256 {local_digest})"
            );
            return false;
        }

        log::info!(
            "remote server binary source: locally built, identical to the copy already on \
             the remote (sha256 {local_digest}) — skipping upload"
        );

        // Recorded only once both copies could be pinned down, so that a remote
        // whose `stat` this does not understand keeps comparing digests rather
        // than trusting a half-specified entry.
        if let Some((local_size, local_mtime_nanos)) = local_stat
            && let Some((remote_size, remote_mtime_secs)) = remote_stat
        {
            super::record_remote_server_install(
                &install_key,
                super::RemoteServerInstall {
                    local_size,
                    local_mtime_nanos,
                    remote_size,
                    remote_mtime_secs,
                    sha256: local_digest,
                    recorded_at: super::now_in_seconds(),
                },
            )
            .await;
        }
        true
    }

    /// The SHA-256 of a file on the remote, as lowercase hex, or `None` if the
    /// remote has no hashing tool this recognizes (or the file is not there).
    ///
    /// Deliberately tolerant: the caller uses this only to skip work it would
    /// otherwise do, so every failure mode is a `debug!` and a `None`.
    #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
    async fn remote_file_sha256(&self, path: &RelPath) -> Option<String> {
        let path = path.display(self.path_style()).into_owned();

        if self.ssh_platform.os.is_windows() {
            let output = self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    "certutil",
                    &["-hashfile", path.as_str(), "SHA256"],
                    true,
                )
                .await;
            return match output {
                Ok(output) => {
                    let digest = super::parse_certutil_hash_output(&output);
                    if digest.is_none() {
                        log::debug!("no SHA-256 in certutil's output for {path}: {output:?}");
                    }
                    digest
                }
                Err(error) => {
                    log::debug!("failed to hash {path} on the remote with certutil: {error:#}");
                    None
                }
            };
        }

        // `sha256sum` is coreutils, so present on Linux; macOS ships `shasum`
        // instead. Both print `<hex>  <path>`.
        let candidates = [
            ("sha256sum", vec![path.clone()]),
            (
                "shasum",
                vec!["-a".to_string(), "256".to_string(), path.clone()],
            ),
        ];
        for (program, args) in candidates {
            match self
                .socket
                .run_command(self.ssh_shell_kind, program, &args, true)
                .await
            {
                Ok(output) => match super::parse_sha256sum_output(&output) {
                    Some(digest) => return Some(digest),
                    None => {
                        log::debug!("no SHA-256 in {program}'s output for {path}: {output:?}")
                    }
                },
                Err(error) => {
                    log::debug!("failed to hash {path} on the remote with {program}: {error:#}")
                }
            }
        }
        None
    }

    /// Deletes the previous generations of this channel's server binary — and
    /// the temporary files an interrupted install left behind — from the
    /// remote's `~/.zed_server`, keeping only `keep_file_name`, the binary
    /// about to be installed there.
    ///
    /// A dev build's binary name carries the version it was built from, so
    /// every connection from a rebuilt client installs a *new* half-gigabyte
    /// file beside the old ones and nothing ever removes them. A 31GB remote
    /// filled up that way on 2026-08-30, and the only symptom the user saw was
    /// that the extraction step failed. Deleting the old generations right
    /// before uploading a new one also means the upload itself has the space:
    /// unlinking a file a running `remote_server` still holds open is safe on
    /// POSIX, and on Windows the delete of a running image simply fails and is
    /// skipped.
    ///
    /// Deliberately not called on the path where the remote already has the
    /// right binary — that connection needs no extra SSH round trip — so the
    /// cleanup happens once per *install*, not once per connection.
    ///
    /// The binaries this fork installed under its earlier names
    /// (`legacy_binary_name_prefixes`) are removed too, but only those with the
    /// fork's `-jp<revision>` ending: upstream Zed installs its own server in
    /// the same directory under the same earlier prefix.
    ///
    /// Best effort: failing to tidy up must not stop a connection that would
    /// otherwise work, so the error is logged rather than propagated.
    async fn remove_stale_server_files(
        &self,
        binary_name_prefix: &str,
        legacy_binary_name_prefixes: &[String],
        keep_file_name: &str,
    ) {
        let dir = remote_server_dir_relative().display(self.path_style());
        let script = if self.ssh_platform.os.is_windows() {
            stale_server_files_cleanup_script_windows(
                &dir,
                binary_name_prefix,
                legacy_binary_name_prefixes,
                keep_file_name,
            )
        } else {
            stale_server_files_cleanup_script_posix(
                &dir,
                binary_name_prefix,
                legacy_binary_name_prefixes,
                keep_file_name,
            )
        };
        let script = match script {
            Ok(script) => script,
            Err(error) => {
                log::warn!("not cleaning up stale remote server files: {error:#}");
                return;
            }
        };

        let (shell_kind, program) = if self.ssh_platform.os.is_windows() {
            (ShellKind::Pwsh, "powershell")
        } else {
            (ShellKind::Posix, "sh")
        };
        let args = shell_kind.args_for_shell(false, script);
        match self
            .socket
            .run_command(self.ssh_shell_kind, program, &args, true)
            .await
        {
            Ok(_) => log::info!(
                "remote server binary cleanup: removed the generations of {dir}/{binary_name_prefix}* \
                 other than {keep_file_name}, and any leftover download temporaries"
            ),
            Err(error) => log::warn!("failed to clean up stale remote server files: {error:#}"),
        }
    }

    /// Deletes the server binaries of every release channel *other* than
    /// `own_binary_prefix`'s from the remote's `~/.zed_server`, returning the
    /// names it removed.
    ///
    /// [`Self::remove_stale_server_files`] deliberately sweeps a single channel:
    /// a host connected to from both a stable and a dev client wants to keep
    /// both binaries, and a cross-channel sweep would make those two clients
    /// take turns re-uploading half a gigabyte each time they alternate. That
    /// preference is worth exactly as much as the disk it occupies, so this runs
    /// only once the pre-flight check has found the remote too full for the
    /// transfer that is about to start — where the alternative is refusing the
    /// connection outright.
    ///
    /// Unlinking a binary that a running `remote_server` of that other channel
    /// still has open is safe on POSIX, which keeps the inode alive until the
    /// process exits; on Windows the delete fails and that binary stays.
    ///
    /// Best effort, like the stale sweep: the caller re-measures the free space
    /// and decides on the figure, not on this having worked.
    async fn remove_other_channel_server_binaries(&self, own_binary_prefix: &str) -> Vec<String> {
        let dir = remote_server_dir_relative().display(self.path_style());
        let script = if self.ssh_platform.os.is_windows() {
            other_channel_server_binaries_cleanup_script_windows(&dir, own_binary_prefix)
        } else {
            other_channel_server_binaries_cleanup_script_posix(&dir, own_binary_prefix)
        };
        let script = match script {
            Ok(script) => script,
            Err(error) => {
                log::warn!("not reclaiming space from the other channels: {error:#}");
                return Vec::new();
            }
        };

        let (shell_kind, program) = if self.ssh_platform.os.is_windows() {
            (ShellKind::Pwsh, "powershell")
        } else {
            (ShellKind::Posix, "sh")
        };
        let args = shell_kind.args_for_shell(false, script);
        match self
            .socket
            .run_command(self.ssh_shell_kind, program, &args, true)
            .await
        {
            Ok(output) => {
                let removed = output
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if !removed.is_empty() {
                    log::info!(
                        "reclaimed space in {dir} by removing the other channels' server \
                         binaries: {}",
                        removed.join(", ")
                    );
                }
                removed
            }
            Err(error) => {
                log::warn!("failed to remove the other channels' server binaries: {error:#}");
                Vec::new()
            }
        }
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path.parent() {
            self.create_remote_dir(parent).await?;
        }

        delegate.set_status(
            Some(i18n::t!("Downloading remote development server on host")),
            cx,
        );

        let connection_timeout = self
            .socket
            .connection_options
            .connection_timeout
            .unwrap_or(10)
            .to_string();

        match self
            .socket
            .run_command(
                self.ssh_shell_kind,
                "curl",
                &[
                    "-f",
                    "-L",
                    "--connect-timeout",
                    &connection_timeout,
                    url,
                    "-o",
                    &tmp_path.display(self.path_style()),
                ],
                true,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if self
                    .socket
                    .run_command(self.ssh_shell_kind, "which", &["curl"], true)
                    .await
                    .is_ok()
                {
                    return Err(e);
                }

                log::info!("curl is not available, trying wget");
                match self
                    .socket
                    .run_command(
                        self.ssh_shell_kind,
                        "wget",
                        &[
                            "--connect-timeout",
                            &connection_timeout,
                            "--tries",
                            "1",
                            url,
                            "-O",
                            &tmp_path.display(self.path_style()),
                        ],
                        true,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        if self
                            .socket
                            .run_command(self.ssh_shell_kind, "which", &["wget"], true)
                            .await
                            .is_ok()
                        {
                            return Err(e);
                        } else {
                            anyhow::bail!(i18n::t!("Neither curl nor wget is available"));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Uploads a local server binary (or a `.gz` / `.zip` archive of one) to a
    /// temporary path on the remote and unpacks it into `dst_path`. The temporary
    /// name keeps the source file's extension, because that is what
    /// `extract_server_binary` dispatches on.
    async fn upload_and_install_server_binary(
        &self,
        src_path: &Path,
        dst_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let file_name = src_path
            .file_name()
            .with_context(|| {
                i18n::t!("The remote development server path has no file name: {src_path:?}")
            })?
            .to_string_lossy()
            .into_owned();
        let tmp_path = remote_server_dir_relative().join(
            RelPath::from_unix_str(&format!("download-{}-{}", std::process::id(), file_name))
                .context("building the remote temporary path for the server binary")?,
        );
        self.upload_local_server_binary(src_path, &tmp_path, delegate, cx)
            .await?;
        self.extract_server_binary(dst_path, &tmp_path, delegate, cx)
            .await
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path.parent() {
            self.create_remote_dir(parent).await?;
        }

        let src_stat = fs::metadata(&src_path).await.with_context(|| {
            i18n::t!("Could not read the remote development server file: {src_path:?}")
        })?;
        let size = src_stat.len();

        if let Some(free_space) = self.remote_free_space_bytes().await
            && insufficient_free_space_message(size, free_space).is_some()
        {
            // Before refusing the connection over disk, give up the one thing on
            // the remote that is ours to give up. The channel is read from the
            // app rather than passed in because it is the same value
            // `ensure_server_binary` derives its own names from, and this is
            // also on the path that installs slirp4netns, which has no channel
            // of its own.
            let own_binary_prefix =
                server_binary_name_prefix(cx.update(|cx| ReleaseChannel::global(cx)));
            let removed = self
                .remove_other_channel_server_binaries(&own_binary_prefix)
                .await;
            let free_space_after = self.remote_free_space_bytes().await.unwrap_or(free_space);
            if let Some(message) = insufficient_free_space_message(size, free_space_after) {
                if removed.is_empty() {
                    anyhow::bail!(message);
                }
                anyhow::bail!(i18n::t!(
                    "{message}\n\
                     Remote servers of other release channels ({removed}) were removed \
                     automatically, but there is still not enough space.",
                    removed = removed.join(", ")
                ));
            }
            log::info!(
                "the remote had {} MB free, too little for a {} MB transfer; after removing [{}] \
                 it has {} MB and the transfer can go ahead",
                free_space / BYTES_PER_MEGABYTE,
                size / BYTES_PER_MEGABYTE,
                removed.join(", "),
                free_space_after / BYTES_PER_MEGABYTE,
            );
        }

        let t0 = Instant::now();
        // The size is in the status because this is by far the longest step of a
        // first connection after the client was rebuilt (the binary's name carries
        // the version, so a new build is always absent from the remote), and a
        // spinner with no indication of scale is what made that wait look like the
        // connection had stalled or restarted.
        delegate.set_status(
            Some(&i18n::t!(
                "Uploading remote development server ({size})",
                size = format_transfer_size(size)
            )),
            cx,
        );
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path,
            size / 1024
        );
        if let Err(error) = self.upload_file(src_path, tmp_path).await {
            // The same reasoning as a failed extraction: a full remote disk is
            // the likeliest cause, and it is the one thing the transport's own
            // message never names.
            return Err(error
                .context(self.free_space_hint().await)
                .context(i18n::t!("failed to upload server binary")));
        }
        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    /// `mkdir -p` on the remote, with the failure explained in Japanese.
    ///
    /// A `~/.zed_server` that cannot be created (a read-only or full home, a
    /// quota) is the first thing to go wrong on a connection and used to
    /// surface as a bare non-zero exit status.
    async fn create_remote_dir(&self, dir: &RelPath) -> Result<()> {
        let dir = dir.display(self.path_style());
        let result = self
            .socket
            .run_command(self.ssh_shell_kind, "mkdir", &["-p", dir.as_ref()], true)
            .await;
        match result {
            Ok(_) => Ok(()),
            // `mkdir` on Windows fails when the directory already exists, which
            // is the ordinary case here, and it has no `-p` to say otherwise.
            Err(error) if self.ssh_platform.os.is_windows() => {
                log::debug!("ignoring mkdir failure on a Windows remote for {dir}: {error:#}");
                Ok(())
            }
            Err(error) => Err(error.context(i18n::t!(
                "Could not create a directory on the remote \
                 (check the free disk space and the permissions): {dir}"
            ))),
        }
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some(i18n::t!("Extracting remote development server")), cx);

        let result = if self.ssh_platform.os.is_windows() {
            self.extract_server_binary_windows(dst_path, tmp_path).await
        } else {
            self.extract_server_binary_posix(dst_path, tmp_path).await
        };
        let Err(error) = result else {
            return Ok(());
        };

        // Unpacking needs as much room again as the archive already occupies,
        // so a full remote disk is by far the likeliest reason to be here — and
        // that is exactly what happened on 2026-08-30, with nothing but
        // `gunzip: write error` to show for it. Leaving the archive and the
        // half-written binary behind would also make the next attempt start
        // from even less room than this one had.
        self.remove_extract_leftovers(tmp_path).await;
        Err(error.context(self.free_space_hint().await))
    }

    /// Removes what a failed extraction leaves in `~/.zed_server`: the uploaded
    /// archive, plus the partially written file (POSIX) or directory (Windows)
    /// the unpacker had started producing beside it. Best effort — the caller
    /// is already returning an error, and this must not replace it.
    async fn remove_extract_leftovers(&self, tmp_path: &RelPath) {
        let tmp_path = tmp_path.display(self.path_style());
        let leftovers = [
            Some(tmp_path.as_ref()),
            tmp_path
                .strip_suffix(".gz")
                .or_else(|| tmp_path.strip_suffix(".zip")),
        ];
        let leftovers = leftovers.into_iter().flatten();

        let (shell_kind, program, script) = if self.ssh_platform.os.is_windows() {
            let paths = leftovers
                .map(powershell_single_quoted)
                .collect::<Vec<_>>()
                .join(", ");
            (
                ShellKind::Pwsh,
                "powershell",
                // `exit 0` for the same reason as the stale-file cleanup: a
                // path that was already gone must not read as a failed command.
                format!(
                    "Remove-Item -LiteralPath {paths} -Force -Recurse \
                     -ErrorAction SilentlyContinue; exit 0"
                ),
            )
        } else {
            let shell_kind = ShellKind::Posix;
            let mut script = String::from("rm -rf --");
            for path in leftovers {
                let Some(path) = shell_kind.try_quote(path) else {
                    log::warn!("not removing the failed extraction's leftovers: {path:?}");
                    return;
                };
                script.push(' ');
                script.push_str(&path);
            }
            (shell_kind, "sh", script)
        };

        let args = shell_kind.args_for_shell(false, script);
        if let Err(error) = self
            .socket
            .run_command(self.ssh_shell_kind, program, &args, true)
            .await
        {
            log::warn!("failed to remove the failed extraction's leftovers: {error:#}");
        }
    }

    /// The Japanese hint appended to a failed transfer or extraction, naming
    /// how much room the remote has left. This error reaches the
    /// connection-failure dialog, which renders the whole `anyhow` chain, so it
    /// is user-facing text and is written in Japanese (ADR-F07).
    async fn free_space_hint(&self) -> String {
        match self.remote_free_space().await {
            Some(free_space) => {
                i18n::t!("The remote may be out of disk space (currently: {free_space})")
            }
            None => i18n::t!(
                "The remote may be out of disk space (the free space could not be determined)"
            )
            .to_string(),
        }
    }

    /// Asks the remote how much room is left where the server binary goes, as
    /// text for a human.
    async fn remote_free_space(&self) -> Option<String> {
        if self.ssh_platform.os.is_windows() {
            let free_space = self
                .windows_free_space("'{0:N1} GB' -f ($free / 1GB)")
                .await?;
            return Some(i18n::t!("{free_space} free"));
        }
        parse_df_free_space(&self.posix_df("-Ph").await?)
    }

    /// The same measurement in bytes, for the pre-flight check that refuses a
    /// transfer the remote cannot hold.
    async fn remote_free_space_bytes(&self) -> Option<u64> {
        if self.ssh_platform.os.is_windows() {
            return self.windows_free_space("$free").await?.trim().parse().ok();
        }
        // `-k` is POSIX, unlike `-h`, and is what makes the figure parseable.
        parse_df_available_kilobytes(&self.posix_df("-Pk").await?)
            .map(|kilobytes| kilobytes.saturating_mul(1024))
    }

    /// Runs `df` for `~/.zed_server`, falling back to the home directory when
    /// that path does not exist yet.
    ///
    /// The fallback is what makes this usable *before* the first install: `df`
    /// exits non-zero on a missing path, so the case where free space matters
    /// most — a first connection that has nothing there yet — used to report
    /// that it could not be read at all.
    async fn posix_df(&self, flags: &str) -> Option<String> {
        for target in free_space_probe_targets(self.path_style()) {
            match self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    "df",
                    // `-P` pins the POSIX one-line-per-filesystem layout `df`
                    // otherwise wraps for a long device name.
                    &[flags.to_string(), target.clone()],
                    true,
                )
                .await
            {
                Ok(output) => return Some(output),
                Err(error) => {
                    log::warn!(
                        "failed to read the remote's free disk space at {target}: {error:#}"
                    );
                }
            }
        }
        None
    }

    /// The Windows counterpart of [`Self::posix_df`]: `render` is a PowerShell
    /// expression over `$free`, the free bytes of the drive holding the server
    /// directory (or the home directory, when the server directory is not there
    /// yet).
    async fn windows_free_space(&self, render: &str) -> Option<String> {
        let script = windows_free_space_script(self.path_style(), render);
        let args = ShellKind::Pwsh.args_for_shell(false, script);
        match self
            .socket
            .run_command(self.ssh_shell_kind, "powershell", &args, true)
            .await
        {
            Ok(output) => {
                let output = output.trim();
                (!output.is_empty()).then(|| output.to_string())
            }
            Err(error) => {
                log::warn!("failed to read the remote's free disk space: {error:#}");
                None
            }
        }
    }

    async fn extract_server_binary_posix(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
    ) -> Result<()> {
        let shell_kind = ShellKind::Posix;
        let server_mode = 0o755;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let server_mode = format!("{:o}", server_mode);
        let server_mode = shell_kind
            .try_quote(&server_mode)
            .context("shell quoting")?;
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;
        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".gz") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(&tmp_path).context("shell quoting")?;
            format!(
                "gunzip -f {orig_tmp_path} && chmod {server_mode} {tmp_path} && mv {tmp_path} {dst_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("chmod {server_mode} {orig_tmp_path} && mv {orig_tmp_path} {dst_path}",)
        };
        let args = shell_kind.args_for_shell(false, script.to_string());
        self.socket
            .run_command(self.ssh_shell_kind, "sh", &args, true)
            .await?;
        Ok(())
    }

    async fn extract_server_binary_windows(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
    ) -> Result<()> {
        let shell_kind = ShellKind::Pwsh;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;

        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".zip") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(tmp_path).context("shell quoting")?;
            let tmp_exe_path = format!("{tmp_path}\\remote_server.exe");
            let tmp_exe_path = shell_kind
                .try_quote(&tmp_exe_path)
                .context("shell quoting")?;
            format!(
                "Expand-Archive -Force -Path {orig_tmp_path} -DestinationPath {tmp_path} -ErrorAction Stop; Move-Item -Force {tmp_exe_path} {dst_path}; Remove-Item -Force {tmp_path} -Recurse; Remove-Item -Force {orig_tmp_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("Move-Item -Force {orig_tmp_path} {dst_path}")
        };

        let args = shell_kind.args_for_shell(false, script);
        self.socket
            .run_command(self.ssh_shell_kind, "powershell", &args, true)
            .await?;
        Ok(())
    }

    fn build_scp_command(
        &self,
        src_path: &Path,
        dest_path_str: &str,
        additional_args: Option<&[&str]>,
    ) -> util::command::Command {
        /// These arguments exist for `ssh` but don't exist / don't have the same semantic for `scp`.
        const SSH_DENY_ARGS_FOR_SCP: &[&str] = &["-X", "-Y"];

        let mut command = util::command::new_command("scp");
        self.socket
            .ssh_options(&mut command, false, Some(SSH_DENY_ARGS_FOR_SCP))
            .args(
                self.socket
                    .connection_options
                    .port
                    .map(|port| vec!["-P".to_string(), port.to_string()])
                    .unwrap_or_default(),
            );
        if let Some(args) = additional_args {
            command.args(args);
        }
        command.arg(src_path).arg(format!(
            "{}:{}",
            self.socket.connection_options.scp_destination(),
            dest_path_str
        ));
        command
    }

    fn build_sftp_command(&self) -> util::command::Command {
        // these arguments exist for "ssh" but don't exist / don't have the same semantic for "sftp"
        const SSH_DENY_ARGS_FOR_SFTP: &[&str] = &["-X", "-Y"];

        let mut command = util::command::new_command("sftp");
        self.socket
            .ssh_options(&mut command, false, Some(SSH_DENY_ARGS_FOR_SFTP))
            .args(
                self.socket
                    .connection_options
                    .port
                    .map(|port| vec!["-P".to_string(), port.to_string()])
                    .unwrap_or_default(),
            );
        command.arg("-b").arg("-");
        command.arg(self.socket.connection_options.scp_destination());
        command.stdin(Stdio::piped());
        command
    }

    async fn upload_file(&self, src_path: &Path, dest_path: &RelPath) -> Result<()> {
        log::debug!("uploading file {:?} to {:?}", src_path, dest_path);

        let src_path_display = src_path.display().to_string();
        let dest_path_str = dest_path.display(self.path_style());

        // We will try SFTP first, and if that fails, we will fall back to SCP.
        // If SCP fails also, we give up and return an error.
        // The reason we allow a fallback from SFTP to SCP is that if the user has to specify a password,
        // depending on the implementation of SSH stack, SFTP may disable interactive password prompts in batch mode.
        // This is for example the case on Windows as evidenced by this implementation snippet:
        // https://github.com/PowerShell/openssh-portable/blob/b8c08ef9da9450a94a9c5ef717d96a7bd83f3332/sshconnect2.c#L417
        let mut sftp_failure = None;
        if Self::is_sftp_available().await {
            log::debug!("using SFTP for file upload");
            let mut command = self.build_sftp_command();
            let sftp_batch = sftp_put_command(&src_path_display, &dest_path_str);

            let mut child = command.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use futures::AsyncWriteExt;
                stdin.write_all(sftp_batch.as_bytes()).await?;
                stdin.flush().await?;
            }

            let output = child.output().await?;
            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!(
                "failed to upload file via SFTP {src_path_display} -> {dest_path_str}: {stderr}"
            );
            // Kept for the final error: SFTP is the transport that usually
            // fails first, and its reason (a full disk, a refused permission)
            // is often the only one that names the real problem — SCP's own
            // message can be as unhelpful as a broken pipe.
            sftp_failure = Some(stderr.trim().to_string());
        }

        log::debug!("using SCP for file upload");
        let mut command = self.build_scp_command(src_path, &dest_path_str, None);
        let output = command.output().await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "failed to upload file via SCP {src_path_display} -> {dest_path_str}: {stderr}",
        );
        // `No space left on device` and `Permission denied` both arrive here,
        // and this text reaches the connection-failure dialog (ADR-F07).
        anyhow::bail!(i18n::t!(
            "Failed to upload the file to the remote ({src_path_display} -> {dest_path_str}): \
             SCP: {scp}{sftp}",
            scp = stderr.trim(),
            sftp = sftp_failure
                .map(|reason| format!(" / SFTP: {reason}"))
                .unwrap_or_default(),
        ));
    }

    async fn is_sftp_available() -> bool {
        which::which("sftp").is_ok()
    }
}

impl SshSocket {
    #[cfg(not(windows))]
    async fn new(options: SshConnectionOptions, socket_path: PathBuf) -> Result<Self> {
        Ok(Self {
            connection_options: options,
            envs: HashMap::default(),
            socket_path,
        })
    }

    #[cfg(windows)]
    async fn new(
        options: SshConnectionOptions,
        password: askpass::EncryptedPassword,
        executor: gpui::BackgroundExecutor,
    ) -> Result<Self> {
        let mut envs = HashMap::default();
        let get_password =
            move |_| Task::ready(std::ops::ControlFlow::Continue(Ok(password.clone())));

        let _proxy = askpass::PasswordProxy::new(Box::new(get_password), executor).await?;
        envs.insert("SSH_ASKPASS_REQUIRE".into(), "force".into());
        envs.insert(
            "SSH_ASKPASS".into(),
            _proxy.script_path().as_ref().display().to_string(),
        );
        envs.insert(
            "ZED_ASKPASS_SOCKET".into(),
            _proxy.socket_path().as_ref().display().to_string(),
        );

        Ok(Self {
            connection_options: options,
            envs,
            _proxy,
        })
    }

    // :WARNING: ssh unquotes arguments when executing on the remote :WARNING:
    // e.g. $ ssh host sh -c 'ls -l' is equivalent to $ ssh host sh -c ls -l
    // and passes -l as an argument to sh, not to ls.
    // Furthermore, some setups (e.g. Coder) will change directory when SSH'ing
    // into a machine. You must use `cd` to get back to $HOME.
    // You need to do it like this: $ ssh host "cd; sh -c 'ls -l /tmp'"
    fn ssh_command(
        &self,
        shell_kind: ShellKind,
        program: &str,
        args: &[impl AsRef<str>],
        allow_pseudo_tty: bool,
    ) -> util::command::Command {
        let mut command = util::command::new_command("ssh");
        let program = shell_kind.prepend_command_prefix(program);
        let mut to_run = shell_kind
            .try_quote_prefix_aware(&program)
            .expect("shell quoting")
            .into_owned();
        for arg in args {
            // We're trying to work with: sh, bash, zsh, fish, tcsh, ...?
            debug_assert!(
                !arg.as_ref().contains('\n'),
                "multiline arguments do not work in all shells"
            );
            to_run.push(' ');
            to_run.push_str(&shell_kind.try_quote(arg.as_ref()).expect("shell quoting"));
        }
        let to_run = if shell_kind == ShellKind::Cmd {
            to_run // 'cd' prints the current directory in CMD
        } else {
            let separator = shell_kind.sequential_commands_separator();
            format!("cd{separator} {to_run}")
        };
        self.ssh_options(&mut command, true, None)
            .arg(self.connection_options.ssh_destination());
        if !allow_pseudo_tty {
            command.arg("-T");
        }
        command.arg(to_run);
        log::debug!("ssh {:?}", command);
        command
    }

    async fn run_command(
        &self,
        shell_kind: ShellKind,
        program: &str,
        args: &[impl AsRef<str>],
        allow_pseudo_tty: bool,
    ) -> Result<String> {
        let mut command = self.ssh_command(shell_kind, program, args, allow_pseudo_tty);
        let output = command.output().await?;
        log::debug!("{:?}: {:?}", command, output);
        anyhow::ensure!(
            output.status.success(),
            "failed to run command {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn ssh_options<'a>(
        &self,
        command: &'a mut util::command::Command,
        include_port_forwards: bool,
        deny_args: Option<&[&str]>,
    ) -> &'a mut util::command::Command {
        let mut args = if include_port_forwards {
            self.connection_options.additional_args()
        } else {
            self.connection_options.additional_args_for_scp()
        };

        // draining all arguments that are explicitly denied
        if let Some(deny_args) = deny_args {
            args.retain(|x| !deny_args.contains(&x.as_str()));
        }

        let cmd = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(args);

        if cfg!(windows) {
            cmd.envs(self.envs.clone());
        }
        #[cfg(not(windows))]
        {
            cmd.args(["-o", "ControlMaster=no", "-o"])
                .arg(format!("ControlPath={}", self.socket_path.display()));
        }
        cmd
    }

    // Returns the SSH command-line options (without the destination) for building commands.
    // On Linux, this includes the ControlPath option to reuse the existing connection.
    // Note: The destination must be added separately after all options to ensure proper
    // SSH command structure: ssh [options] destination [command]
    fn ssh_command_options(&self) -> Vec<String> {
        let arguments = self.connection_options.additional_args();
        #[cfg(not(windows))]
        let arguments = {
            let mut args = arguments;
            args.extend(vec![
                "-o".to_string(),
                "ControlMaster=no".to_string(),
                "-o".to_string(),
                format!("ControlPath={}", self.socket_path.display()),
            ]);
            args
        };
        arguments
    }

    async fn environment_posix(&self) -> Result<RemoteEnvironment> {
        let script = RemoteEnvironment::posix_script();

        let output = self
            .run_command(ShellKind::Posix, "sh", &["-c", &script], false)
            .await
            .context("Failed to determine remote environment")?;
        RemoteEnvironment::parse_posix(&output)
    }

    /// Best-effort detection of the remote OS version. Failures are logged and
    /// result in `None` rather than failing the connection, since this is only
    /// used for telemetry.
    async fn os_version(&self, os: RemoteOs, shell: ShellKind) -> Option<String> {
        let (program, args) = super::os_version_command(os);
        match self.run_command(shell, program, args, false).await {
            Ok(output) => super::parse_os_version(os, &output),
            Err(error) => {
                log::warn!("Failed to determine remote OS version: {error:#}");
                None
            }
        }
    }

    async fn home_dir(&self, shell: ShellKind) -> Option<String> {
        match self
            .run_command(shell, HOME_DIR_PROGRAM, &HOME_DIR_ARGS, false)
            .await
        {
            Ok(output) => {
                let home_dir = parse_home_dir(&output);
                if home_dir.is_none() {
                    log::warn!("Failed to parse remote home directory from {output:?}");
                }
                home_dir
            }
            Err(error) => {
                log::warn!("Failed to determine remote home directory: {error:#}");
                None
            }
        }
    }

    async fn platform_windows(&self, shell: ShellKind) -> Result<RemotePlatform> {
        let output = self
            .run_command(
                shell,
                "cmd.exe",
                &["/c", "echo", "%PROCESSOR_ARCHITECTURE%"],
                false,
            )
            .await
            .context(
                "Failed to run 'echo %PROCESSOR_ARCHITECTURE%' to determine Windows architecture",
            )?;

        Ok(RemotePlatform {
            os: RemoteOs::Windows,
            arch: match output.trim() {
                "AMD64" => RemoteArch::X86_64,
                "ARM64" => RemoteArch::Aarch64,
                arch => anyhow::bail!(
                    "Prebuilt remote servers are not yet available for windows-{arch}. See https://zed.dev/docs/remote-development"
                ),
            },
        })
    }

    /// Asks a POSIX remote everything in [`PosixProbe`] at once, or returns
    /// `None` when the answers do not come back in the expected shape — which is
    /// what a Windows remote does, since neither `cmd.exe` nor PowerShell can run
    /// this script. The caller then falls back to the one-command-at-a-time
    /// sequence, so `None` costs a round trip and changes nothing else.
    ///
    /// Acceptance hangs on `uname` parsing rather than on the exit status: a
    /// Windows remote that happens to have a POSIX `sh` on `$PATH` (Git for
    /// Windows) would run the script and answer `MINGW64_NT-…`, which
    /// [`parse_platform`] rejects.
    async fn probe_posix(
        &self,
        server_binary_path: &str,
        slirp4netns_path: &str,
    ) -> Option<PosixProbe> {
        let shell_kind = ShellKind::Posix;
        let script = posix_probe_script(shell_kind, server_binary_path, slirp4netns_path)?;
        let output = self
            .run_command(
                shell_kind,
                "sh",
                &shell_kind.args_for_shell(false, script),
                false,
            )
            .await
            .map_err(|error| log::debug!("the batched POSIX probe did not run: {error:#}"))
            .ok()?;
        parse_posix_probe(&output, server_binary_path)
    }

    /// Probes whether the remote host is running Windows.
    ///
    /// This is done by attempting to run a simple Windows-specific command.
    /// If it succeeds and returns Windows-like output, we assume it's Windows.
    async fn probe_is_windows(&self) -> bool {
        match self
            .run_command(ShellKind::Cmd, "cmd.exe", &["/c", "ver"], false)
            .await
        {
            // Windows 'ver' command outputs something like "Microsoft Windows [Version 10.0.19045.5011]"
            Ok(output) => output.trim().contains("indows"),
            Err(_) => false,
        }
    }

    async fn environment_windows(&self) -> Result<RemoteEnvironment> {
        use base64::Engine as _;

        let script = RemoteEnvironment::windows_script();
        // The SSH shell is still unknown, encode the script to avoid quoting issues.
        let encoded_script = base64::engine::general_purpose::STANDARD.encode(
            script
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        match self
            .run_command(
                ShellKind::Cmd,
                "powershell",
                &["-E", &encoded_script],
                false,
            )
            .await
        {
            Ok(output) => RemoteEnvironment::parse_windows(&output),
            Err(error) => {
                log::error!("Failed to determine remote Windows environment: {error:#}");
                let platform = self.platform_windows(ShellKind::Cmd).await?;
                let os_version = self.os_version(RemoteOs::Windows, ShellKind::Cmd).await;
                return Ok(RemoteEnvironment {
                    shell: "cmd.exe".to_owned(),
                    platform,
                    os_version,
                });
            }
        }
    }
}

fn parse_port_number(port_str: &str) -> Result<u16> {
    port_str
        .parse()
        .with_context(|| format!("parsing port number: {port_str}"))
}

fn split_port_forward_tokens(spec: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = spec.chars().peekable();

    while chars.peek().is_some() {
        if chars.peek() == Some(&'[') {
            chars.next();
            let mut bracket_content = String::new();
            loop {
                match chars.next() {
                    Some(']') => break,
                    Some(ch) => bracket_content.push(ch),
                    None => anyhow::bail!("Unmatched '[' in port forward spec: {spec}"),
                }
            }
            tokens.push(bracket_content);
            if chars.peek() == Some(&':') {
                chars.next();
            }
        } else {
            let mut token = String::new();
            for ch in chars.by_ref() {
                if ch == ':' {
                    break;
                }
                token.push(ch);
            }
            tokens.push(token);
        }
    }

    Ok(tokens)
}

fn parse_port_forward_spec(spec: &str) -> Result<SshPortForwardOption> {
    let tokens = if spec.contains('[') {
        split_port_forward_tokens(spec)?
    } else {
        spec.split(':').map(String::from).collect()
    };

    match tokens.len() {
        4 => {
            let local_port = parse_port_number(&tokens[1])?;
            let remote_port = parse_port_number(&tokens[3])?;

            Ok(SshPortForwardOption {
                local_host: Some(tokens[0].clone()),
                local_port,
                remote_host: Some(tokens[2].clone()),
                remote_port,
            })
        }
        3 => {
            let local_port = parse_port_number(&tokens[0])?;
            let remote_port = parse_port_number(&tokens[2])?;

            Ok(SshPortForwardOption {
                local_host: None,
                local_port,
                remote_host: Some(tokens[1].clone()),
                remote_port,
            })
        }
        _ => anyhow::bail!("Invalid port forward format: {spec}"),
    }
}

impl SshConnectionOptions {
    pub fn parse_command_line(input: &str) -> Result<Self> {
        let input = input.trim_start_matches("ssh ");
        let mut hostname: Option<String> = None;
        let mut username: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut args = Vec::new();
        let mut port_forwards: Vec<SshPortForwardOption> = Vec::new();

        // disallowed: -E, -e, -F, -f, -G, -g, -M, -N, -n, -O, -q, -S, -s, -T, -t, -V, -v, -W
        const ALLOWED_OPTS: &[&str] = &[
            "-4", "-6", "-A", "-a", "-C", "-K", "-k", "-X", "-x", "-Y", "-y",
        ];
        const ALLOWED_ARGS: &[&str] = &[
            "-B", "-b", "-c", "-D", "-F", "-I", "-i", "-J", "-l", "-m", "-o", "-P", "-p", "-R",
            "-w",
        ];

        let mut tokens = ShellKind::Posix
            .split(input)
            .context("invalid input")?
            .into_iter();

        'outer: while let Some(arg) = tokens.next() {
            if ALLOWED_OPTS.contains(&(&arg as &str)) {
                args.push(arg.to_string());
                continue;
            }
            if arg == "-p" {
                port = tokens.next().and_then(|arg| arg.parse().ok());
                continue;
            } else if let Some(p) = arg.strip_prefix("-p") {
                port = p.parse().ok();
                continue;
            }
            if arg == "-l" {
                username = tokens.next();
                continue;
            } else if let Some(l) = arg.strip_prefix("-l") {
                username = Some(l.to_string());
                continue;
            }
            if arg == "-L" || arg.starts_with("-L") {
                let forward_spec = if arg == "-L" {
                    tokens.next()
                } else {
                    Some(arg.strip_prefix("-L").unwrap().to_string())
                };

                if let Some(spec) = forward_spec {
                    port_forwards.push(parse_port_forward_spec(&spec)?);
                } else {
                    anyhow::bail!("Missing port forward format");
                }
            }

            for a in ALLOWED_ARGS {
                if arg == *a {
                    args.push(arg);
                    if let Some(next) = tokens.next() {
                        args.push(next);
                    }
                    continue 'outer;
                } else if arg.starts_with(a) {
                    args.push(arg);
                    continue 'outer;
                }
            }
            if arg.starts_with("-") || hostname.is_some() {
                anyhow::bail!("unsupported argument: {:?}", arg);
            }
            let mut input = &arg as &str;
            // Destination might be: username1@username2@ip2@ip1
            if let Some((u, rest)) = input.rsplit_once('@') {
                input = rest;
                username = Some(u.to_string());
            }

            // Handle port parsing, accounting for IPv6 addresses
            // IPv6 addresses can be: 2001:db8::1 or [2001:db8::1]:22
            if input.starts_with('[') {
                if let Some((rest, p)) = input.rsplit_once("]:") {
                    input = rest.strip_prefix('[').unwrap_or(rest);
                    port = p.parse().ok();
                } else if input.ends_with(']') {
                    input = input.strip_prefix('[').unwrap_or(input);
                    input = input.strip_suffix(']').unwrap_or(input);
                }
            } else if let Some((rest, p)) = input.rsplit_once(':')
                && !rest.contains(":")
            {
                input = rest;
                port = p.parse().ok();
            }

            hostname = Some(input.to_string())
        }

        let Some(hostname) = hostname else {
            anyhow::bail!("missing hostname");
        };

        let port_forwards = match port_forwards.len() {
            0 => None,
            _ => Some(port_forwards),
        };

        Ok(Self {
            host: hostname.into(),
            username,
            port,
            port_forwards,
            args: Some(args),
            password: None,
            nickname: None,
            upload_binary_over_ssh: false,
            connection_timeout: None,
        })
    }

    pub fn ssh_destination(&self) -> String {
        let mut result = String::default();
        if let Some(username) = &self.username {
            // Username might be: username1@username2@ip2
            let username = urlencoding::encode(username);
            result.push_str(&username);
            result.push('@');
        }

        result.push_str(&self.host.to_string());
        result
    }

    pub fn additional_args_for_scp(&self) -> Vec<String> {
        self.args.iter().flatten().cloned().collect::<Vec<String>>()
    }

    pub fn additional_args(&self) -> Vec<String> {
        let mut args = self.additional_args_for_scp();

        // After the machine resumes from sleep the TCP connection is dead but
        // `ssh` never notices, so the master, proxy and terminal processes hang
        // forever instead of exiting and letting the client reconnect. Skip it
        // when the user configured their own keepalive, so their value wins.
        let has_server_alive_option = args
            .iter()
            .any(|arg| arg.to_ascii_lowercase().contains("serveralive"));
        if !has_server_alive_option {
            args.extend([
                "-o".to_string(),
                "ServerAliveInterval=15".to_string(),
                "-o".to_string(),
                "ServerAliveCountMax=3".to_string(),
            ]);
        }

        if let Some(timeout) = self.connection_timeout {
            args.extend(["-o".to_string(), format!("ConnectTimeout={}", timeout)]);
        }

        if let Some(port) = self.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }

        if let Some(forwards) = &self.port_forwards {
            args.extend(forwards.iter().map(|pf| {
                let local_host = match &pf.local_host {
                    Some(host) => host,
                    None => "localhost",
                };
                let remote_host = match &pf.remote_host {
                    Some(host) => host,
                    None => "localhost",
                };

                format!(
                    "-L{}:{}:{}:{}",
                    bracket_ipv6(local_host),
                    pf.local_port,
                    bracket_ipv6(remote_host),
                    pf.remote_port
                )
            }));
        }

        args
    }

    fn scp_destination(&self) -> String {
        if let Some(username) = &self.username {
            format!("{}@{}", username, self.host.to_bracketed_string())
        } else {
            self.host.to_string()
        }
    }

    pub fn connection_string(&self) -> String {
        let host = if let Some(port) = &self.port {
            format!("{}:{}", self.host.to_bracketed_string(), port)
        } else {
            self.host.to_string()
        };

        if let Some(username) = &self.username {
            format!("{}@{}", username, host)
        } else {
            host
        }
    }
}

/// A PowerShell single-quoted string literal.
///
/// [`ShellKind::try_quote`] leaves a value with no metacharacters bare, which
/// is right for a cmdlet argument but wrong in expression position: PowerShell
/// parses a bare word on the right of `=` or of `-like` as a command to run, so
/// `$env:HTTP_PROXY=http://127.0.0.1:1` would try to execute the URL. Proxy
/// URLs and file names contain nothing that would force `try_quote` to quote
/// them, so the quoting has to be unconditional.
fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The one-line script behind [`PosixProbe`], or `None` if a path in it cannot
/// be quoted for the remote shell.
///
/// One line because `SshSocket::ssh_command` sends the command as a single
/// argument and not every remote login shell keeps embedded newlines intact.
fn posix_probe_script(
    shell_kind: ShellKind,
    server_binary_path: &str,
    slirp4netns_path: &str,
) -> Option<String> {
    let binary = shell_kind.try_quote(server_binary_path)?;
    let slirp = shell_kind.try_quote(slirp4netns_path)?;
    let separator = PROBE_SECTION_SEPARATOR;
    // Both `stat` spellings are tried because GNU coreutils and BSD (so macOS)
    // disagree on the flags, and both `os-release` and `sw_vers` are asked for
    // the same reason: only the one that exists prints anything. On GNU, `-c`
    // fails only when the file is absent, in which case `-f` fails too, so the
    // filesystem report that `-f` means there never reaches the parser.
    Some(format!(
        "echo \"$SHELL\"; echo {separator}; \
         uname -sm; echo {separator}; \
         cat /etc/os-release 2>/dev/null; sw_vers -productVersion 2>/dev/null; echo {separator}; \
         if {binary} version >/dev/null 2>&1; then echo yes; fi; echo {separator}; \
         stat -c \"%s %Y\" {binary} 2>/dev/null || stat -f \"%z %m\" {binary} 2>/dev/null; \
         echo {separator}; \
         if {slirp} --version >/dev/null 2>&1; then echo yes; fi; echo {separator}; \
         printf ZED_HOME_BEGIN; echo; pwd; printf ZED_HOME_END"
    ))
}

/// Splits the output of [`posix_probe_script`] into a [`PosixProbe`], or `None`
/// if it is not the output of that script at all.
fn parse_posix_probe(output: &str, server_binary_path: &str) -> Option<PosixProbe> {
    // The home directory comes last and is split off whole, since nothing
    // keeps a directory name from containing the separator.
    let mut sections = output.splitn(7, PROBE_SECTION_SEPARATOR);
    let shell = sections.next()?;
    let uname = sections.next()?;
    let os_version = sections.next()?;
    let server_binary = sections.next()?;
    let server_binary_stat = sections.next()?;
    let slirp4netns = sections.next()?;
    let home_dir = sections.next().and_then(parse_home_dir);

    let platform = super::parse_platform(uname)
        .map_err(|error| {
            log::debug!("the batched POSIX probe returned no usable platform: {error:#}")
        })
        .ok()?;

    Some(PosixProbe {
        shell: parse_shell(shell, "sh"),
        platform,
        os_version: super::parse_os_version(platform.os, os_version),
        server_binary_path: server_binary_path.to_owned(),
        server_binary_runs: server_binary.trim() == "yes",
        server_binary_stat: super::parse_remote_stat_output(server_binary_stat),
        slirp4netns_runs: slirp4netns.trim() == "yes",
        home_dir,
    })
}

/// The `(prefix, file name)` the server binary is installed under in the
/// remote's `~/.zed_server`. The prefix is the part that stays the same across
/// every version of this channel that has ever been installed there, which is
/// what [`SshRemoteConnection::remove_stale_server_files`] globs on.
fn server_binary_names(
    release_channel: ReleaseChannel,
    version: &Version,
    is_windows: bool,
) -> (String, String) {
    let prefix = server_binary_name_prefix(release_channel);
    let name = format!(
        "{}{}",
        super::server_binary_name(release_channel, version),
        if is_windows { ".exe" } else { "" }
    );
    (prefix, name)
}

/// What every server binary this build installs begins with, whichever release
/// channel and version installed it.
const SERVER_BINARY_NAME_PREFIX: &str = branding::CURRENT.remote_server_binary_prefix;

/// Renders a byte count for the connection dialog. Binary units, because that is
/// what the rest of the tooling around this file (`ls -lh`, `df -h`) reports.
fn format_transfer_size(bytes: u64) -> String {
    const MIB: f64 = (1024 * 1024) as f64;
    const KIB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0} KB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// Rejects a string that is about to be interpolated into a glob pattern
/// *unquoted* — which is the whole point of the pattern — unless it is inert.
///
/// Every caller passes a compile-time constant today (`.zed_server` from
/// `paths`, and the `branding` prefixes followed by
/// `{ReleaseChannel::dev_name}-`); this is what
/// notices if one of them stops being one.
fn is_inert_glob_fragment(fragment: &str) -> bool {
    !fragment.is_empty()
        && fragment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// The `sh` one-liner behind [`SshRemoteConnection::remove_stale_server_files`].
///
/// Matching is done with two globs rather than a listing because the client
/// keeps no record of which versions it installed on a given host. The loop
/// tests `-e` so that a glob that matched nothing (left literal by `sh`) is
/// skipped, and ends in `exit 0` so that the last iteration's comparison does
/// not decide the script's exit status.
///
/// A name under one of `legacy_binary_name_prefixes` is only removed when what
/// follows its last `-jp` is a non-empty run of digits: the glob narrows the
/// candidates to `…-jp<digit>…`, and the `case` refuses the rest, so upstream
/// Zed's `zed-remote-server-<channel>-<version>` beside them stays.
fn stale_server_files_cleanup_script_posix(
    dir: &str,
    binary_name_prefix: &str,
    legacy_binary_name_prefixes: &[String],
    keep_file_name: &str,
) -> Result<String> {
    let shell_kind = ShellKind::Posix;
    ensure_inert_cleanup_fragments(dir, binary_name_prefix, legacy_binary_name_prefixes)?;
    let keep_path = format!("{dir}/{keep_file_name}");
    let keep = shell_kind.try_quote(&keep_path).context("shell quoting")?;
    let mut script = format!(
        "for stale in {dir}/{binary_name_prefix}* {dir}/download-*; \
         do if [ -e \"$stale\" ] && [ \"$stale\" != {keep} ]; then rm -f -- \"$stale\"; fi; \
         done; "
    );
    if !legacy_binary_name_prefixes.is_empty() {
        let legacy_globs = legacy_binary_name_prefixes
            .iter()
            .map(|prefix| format!("{dir}/{prefix}*-jp[0-9]*"))
            .collect::<Vec<_>>()
            .join(" ");
        script.push_str(&format!(
            "for stale in {legacy_globs}; \
             do if [ -e \"$stale\" ]; then case \"${{stale##*-jp}}\" in \
             ''|*[!0-9]*) ;; *) rm -f -- \"$stale\";; esac; fi; \
             done; "
        ));
    }
    script.push_str("exit 0");
    Ok(script)
}

fn ensure_inert_cleanup_fragments(
    dir: &str,
    binary_name_prefix: &str,
    legacy_binary_name_prefixes: &[String],
) -> Result<()> {
    anyhow::ensure!(
        is_inert_glob_fragment(dir)
            && is_inert_glob_fragment(binary_name_prefix)
            && legacy_binary_name_prefixes
                .iter()
                .all(|prefix| is_inert_glob_fragment(prefix)),
        "refusing to build a cleanup glob out of {dir:?}, {binary_name_prefix:?} \
         and {legacy_binary_name_prefixes:?}"
    );
    Ok(())
}

/// The PowerShell equivalent of [`stale_server_files_cleanup_script_posix`].
///
/// `-ErrorAction SilentlyContinue` on both halves covers the two expected
/// failures: `~/.zed_server` not existing yet, and a binary that a running
/// `remote_server` still has mapped, which Windows refuses to delete.
/// `-Recurse` is there because `Expand-Archive` unpacks into a *directory* next
/// to the archive, which an interrupted install leaves behind under the same
/// `download-` prefix. The trailing `exit 0` is what keeps a first install —
/// where `~/.zed_server` does not exist yet — from looking like a failure:
/// `powershell -C` exits 1 whenever `$?` is false at the end, even for an error
/// `-ErrorAction SilentlyContinue` swallowed.
///
/// A name under one of `legacy_binary_name_prefixes` has to end with
/// `-jp<digits>` or `-jp<digits>.exe` as well, as in the `sh` version. The
/// regular expression ends in `\z` rather than `$` so that no layer of quoting
/// between here and PowerShell has a character of its own to read in it.
fn stale_server_files_cleanup_script_windows(
    dir: &str,
    binary_name_prefix: &str,
    legacy_binary_name_prefixes: &[String],
    keep_file_name: &str,
) -> Result<String> {
    ensure_inert_cleanup_fragments(dir, binary_name_prefix, legacy_binary_name_prefixes)?;
    let dir = powershell_single_quoted(dir);
    let binary_glob = powershell_single_quoted(&format!("{binary_name_prefix}*"));
    let keep = powershell_single_quoted(keep_file_name);
    let legacy_condition = if legacy_binary_name_prefixes.is_empty() {
        String::new()
    } else {
        let legacy_prefix_condition = legacy_binary_name_prefixes
            .iter()
            .map(|prefix| {
                format!(
                    "$_.Name -like {}",
                    powershell_single_quoted(&format!("{prefix}*"))
                )
            })
            .collect::<Vec<_>>()
            .join(" -or ");
        format!(
            " -or (({legacy_prefix_condition}) \
             -and $_.Name -match '-jp[0-9]+(\\.exe)?\\z')"
        )
    };
    Ok(format!(
        "Get-ChildItem -LiteralPath {dir} -Force -ErrorAction SilentlyContinue | \
         Where-Object {{ (($_.Name -like {binary_glob} -or $_.Name -like 'download-*') \
         -and $_.Name -ne {keep}){legacy_condition} }} | \
         Remove-Item -Force -Recurse -ErrorAction SilentlyContinue; exit 0"
    ))
}

/// The `sh` one-liner behind
/// [`SshRemoteConnection::remove_other_channel_server_binaries`]: deletes every
/// server binary in `dir` whose name does *not* start with `own_binary_prefix`,
/// printing one removed name per line.
///
/// The `case` is what expresses "every channel but mine" — `$stale` is compared
/// with its directory stripped so that the pattern only has to describe a file
/// name. Removed names are echoed because the caller has no other way to tell
/// whether the sweep freed anything, and that decides what it tells the user.
fn other_channel_server_binaries_cleanup_script_posix(
    dir: &str,
    own_binary_prefix: &str,
) -> Result<String> {
    anyhow::ensure!(
        is_inert_glob_fragment(dir) && is_inert_glob_fragment(own_binary_prefix),
        "refusing to build a cleanup glob out of {dir:?} and {own_binary_prefix:?}"
    );
    Ok(format!(
        "for stale in {dir}/{SERVER_BINARY_NAME_PREFIX}*; \
         do if [ -e \"$stale\" ]; then \
         case \"${{stale##*/}}\" in {own_binary_prefix}*) ;; \
         *) if rm -f -- \"$stale\"; then echo \"${{stale##*/}}\"; fi;; esac; fi; \
         done; exit 0"
    ))
}

/// The PowerShell equivalent of
/// [`other_channel_server_binaries_cleanup_script_posix`].
///
/// `Remove-Item` says nothing on success, so the name is emitted separately —
/// and only after a delete that did not throw, which is how a binary a running
/// `remote_server` has mapped (undeletable on Windows) stays out of the count.
fn other_channel_server_binaries_cleanup_script_windows(
    dir: &str,
    own_binary_prefix: &str,
) -> Result<String> {
    anyhow::ensure!(
        is_inert_glob_fragment(dir) && is_inert_glob_fragment(own_binary_prefix),
        "refusing to build a cleanup glob out of {dir:?} and {own_binary_prefix:?}"
    );
    let dir = powershell_single_quoted(dir);
    let all_channels_glob = powershell_single_quoted(&format!("{SERVER_BINARY_NAME_PREFIX}*"));
    let own_glob = powershell_single_quoted(&format!("{own_binary_prefix}*"));
    Ok(format!(
        "Get-ChildItem -LiteralPath {dir} -Force -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.Name -like {all_channels_glob} -and -not ($_.Name -like {own_glob}) }} | \
         ForEach-Object {{ try {{ Remove-Item -LiteralPath $_.FullName -Force -Recurse \
         -ErrorAction Stop; $_.Name }} catch {{ }} }}; exit 0"
    ))
}

/// Reads how much room is left out of `df -Ph` output, as a phrase to drop into
/// the Japanese error the connection dialog shows.
///
/// `-P` pins the POSIX layout — one line per filesystem, the columns being
/// `Filesystem Size Used Avail Capacity Mounted-on` — so only the last column
/// can realistically contain a space and counting from the front is sound. The
/// two shape checks are there to reject the header, which is what is left when
/// `df` names a filesystem it then has nothing to say about.
fn parse_df_free_space(output: &str) -> Option<String> {
    let line = output.lines().rev().find(|line| !line.trim().is_empty())?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    let [_filesystem, _size, _used, available, capacity, ..] = fields.as_slice() else {
        return None;
    };
    if !available.starts_with(|c: char| c.is_ascii_digit()) || !capacity.ends_with('%') {
        return None;
    }
    Some(i18n::t!("{available} free, {capacity} used"))
}

/// The directories to ask `df` about, in order.
///
/// `~/.zed_server` does not exist before the first install and `df` exits
/// non-zero on a path that is not there, so without the home directory behind
/// it the free-space figure was unavailable in exactly the situation that needs
/// it: a first connection with no room to unpack into.
fn free_space_probe_targets(path_style: PathStyle) -> [String; 2] {
    [
        remote_server_dir_relative()
            .display(path_style)
            .into_owned(),
        ".".to_string(),
    ]
}

/// The PowerShell counterpart of [`free_space_probe_targets`]: the same
/// fallback, expressed inside the one command a Windows remote runs. `render`
/// is an expression over `$free`, the free bytes of the drive that holds the
/// chosen directory.
fn windows_free_space_script(path_style: PathStyle, render: &str) -> String {
    let dir = remote_server_dir_relative().display(path_style);
    format!(
        "$target = {}; if (-not (Test-Path -LiteralPath $target)) {{ $target = $HOME }}; \
         $free = (Get-Item -LiteralPath $target -ErrorAction Stop).PSDrive.Free; {render}",
        powershell_single_quoted(&dir)
    )
}

/// Refuses a transfer the remote cannot hold, in Japanese, or `None` when it
/// can.
///
/// Unpacking needs as much room again as the archive occupies, so a transfer
/// is only useful with twice the file free. Deciding this *before* spending
/// minutes on the upload turns a failure that used to arrive at the very end as
/// `gunzip: write error` into one the user can act on immediately.
fn insufficient_free_space_message(file_size: u64, free_space: u64) -> Option<String> {
    let required = file_size.saturating_mul(2);
    (free_space < required).then(|| {
        i18n::t!(
            "The remote does not have enough free disk space (about {required} MB needed, {free} MB free). \
             Delete files you no longer need on the remote host, then reconnect.",
            required = required.div_ceil(BYTES_PER_MEGABYTE),
            free = free_space / BYTES_PER_MEGABYTE,
        )
    })
}

/// The `Available` column of `df -Pk` output, in 1K blocks.
///
/// Separate from [`parse_df_free_space`] because that one keeps `df -Ph`'s
/// human suffixes (`1.5G`), which are what a person should read and exactly
/// what arithmetic cannot use.
fn parse_df_available_kilobytes(output: &str) -> Option<u64> {
    let line = output.lines().rev().find(|line| !line.trim().is_empty())?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    let [_filesystem, _size, _used, available, capacity, ..] = fields.as_slice() else {
        return None;
    };
    if !capacity.ends_with('%') {
        return None;
    }
    available.parse().ok()
}

/// The environment variables [`build_command_windows`] forwards to the remote
/// despite Windows OpenSSH's 8K command-line limit, rendered as PowerShell
/// assignments, or `None` when they would cost more of that budget than
/// [`WINDOWS_FORWARDED_ENV_MAX_BYTES`].
///
/// Iterating the allowlist rather than the map keeps the order independent of
/// the hash seed.
fn windows_forwarded_env_assignments(input_env: &HashMap<String, String>) -> Option<String> {
    let separator = ShellKind::PowerShell.sequential_and_commands_separator();
    let mut rendered = String::new();
    for name in WINDOWS_FORWARDED_ENV {
        let Some(value) = input_env.get(*name) else {
            continue;
        };
        rendered.push_str(&format!(
            "$env:{name}={} {separator} ",
            powershell_single_quoted(value)
        ));
    }
    (rendered.len() <= WINDOWS_FORWARDED_ENV_MAX_BYTES).then_some(rendered)
}

fn build_command_posix(
    input_program: Option<String>,
    input_args: &[String],
    input_env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    ssh_env: HashMap<String, String>,
    ssh_path_style: PathStyle,
    ssh_shell: &str,
    ssh_shell_kind: ShellKind,
    ssh_options: Vec<String>,
    ssh_destination: &str,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    let mut exec = posix_working_dir_prefix(working_dir, ssh_path_style, ssh_shell_kind)?;
    write!(exec, "exec env ")?;

    for (k, v) in input_env.iter() {
        let assignment = format!("{k}={v}");
        let assignment = ssh_shell_kind
            .try_quote(&assignment)
            .context("shell quoting")?;
        write!(exec, "{assignment} ")?;
    }

    if let Some(input_program) = input_program {
        write!(
            exec,
            "{}",
            ssh_shell_kind
                .try_quote_prefix_aware(&input_program)
                .context("shell quoting")?
        )?;
        for arg in input_args {
            let arg = ssh_shell_kind.try_quote(&arg).context("shell quoting")?;
            write!(exec, " {arg}")?;
        }
    } else {
        write!(exec, "{ssh_shell} -l")?;
    };

    Ok(ssh_command_template(
        ssh_options,
        port_forward,
        interactive,
        ssh_destination,
        exec,
        ssh_env,
    ))
}

fn posix_working_dir_prefix(
    working_dir: Option<String>,
    ssh_path_style: PathStyle,
    ssh_shell_kind: ShellKind,
) -> Result<String> {
    let mut exec = String::new();
    if let Some(working_dir) = working_dir {
        let working_dir = RemotePathBuf::new(working_dir, ssh_path_style).to_string();

        // For paths starting with ~/, we need $HOME to expand, but the remainder
        // must be properly quoted to prevent command injection.
        // Pattern: cd "$HOME"/'quoted/remainder' - $HOME expands, rest is single-quoted
        const TILDE_PREFIX: &str = "~/";
        if working_dir.starts_with(TILDE_PREFIX) {
            let remainder = working_dir.trim_start_matches(TILDE_PREFIX);
            write!(
                exec,
                "cd {} {} ",
                home_relative_path(ssh_shell_kind, (!remainder.is_empty()).then_some(remainder))?,
                ssh_shell_kind.sequential_and_commands_separator()
            )?;
        } else {
            let quoted_dir = ssh_shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?;
            write!(
                exec,
                "cd {quoted_dir} {} ",
                ssh_shell_kind.sequential_and_commands_separator()
            )?;
        }
    } else {
        write!(
            exec,
            "cd {} ",
            ssh_shell_kind.sequential_and_commands_separator()
        )?;
    };
    Ok(exec)
}

fn ssh_command_template(
    ssh_options: Vec<String>,
    port_forward: Option<(u16, String, u16)>,
    interactive: Interactive,
    ssh_destination: &str,
    exec: String,
    ssh_env: HashMap<String, String>,
) -> CommandTemplate {
    let mut args = Vec::new();
    args.extend(ssh_options);

    if let Some((local_port, host, remote_port)) = port_forward {
        args.push("-L".into());
        args.push(format!(
            "{}:{}:{}",
            local_port,
            bracket_ipv6(&host),
            remote_port
        ));
    }

    // LogLevel=ERROR suppresses the "Connection to ... closed." message while
    // preserving SSH errors.
    args.extend(["-o".into(), "LogLevel=ERROR".into()]);
    match interactive {
        // -t forces pseudo-TTY allocation (for interactive use)
        Interactive::Yes => args.push("-t".into()),
        // -T disables pseudo-TTY allocation (for non-interactive piped stdio)
        Interactive::No => args.push("-T".into()),
    }
    // The destination must come after all options but before the command
    args.push(ssh_destination.into());
    args.push(exec);

    CommandTemplate {
        program: "ssh".into(),
        args,
        env: ssh_env,
    }
}

fn build_command_windows(
    input_program: Option<String>,
    input_args: &[String],
    input_env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    ssh_env: HashMap<String, String>,
    ssh_path_style: PathStyle,
    ssh_shell: &str,
    _ssh_shell_kind: ShellKind,
    ssh_options: Vec<String>,
    ssh_destination: &str,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    use base64::Engine as _;
    use std::fmt::Write as _;

    let mut exec = String::new();
    let shell_kind = ShellKind::PowerShell;

    if let Some(working_dir) = working_dir {
        let working_dir = RemotePathBuf::new(working_dir, ssh_path_style).to_string();

        write!(
            exec,
            "Set-Location -Path {} {} ",
            shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?,
            shell_kind.sequential_and_commands_separator()
        )?;
    }

    // Windows OpenSSH has an 8K character limit for command lines. Sending a lot of environment
    // variables easily puts us over the limit, so all that crosses is the small fixed allowlist in
    // `WINDOWS_FORWARDED_ENV` — see the comment there for why those cannot be dropped like the rest.
    match windows_forwarded_env_assignments(input_env) {
        Some(assignments) => exec.push_str(&assignments),
        None => log::warn!(
            "not forwarding proxy environment variables to a Windows remote: they render to more \
             than {WINDOWS_FORWARDED_ENV_MAX_BYTES} bytes, which risks Windows OpenSSH's 8K \
             command-line limit"
        ),
    }

    if let Some(input_program) = input_program {
        write!(
            exec,
            "{}",
            shell_kind
                .try_quote_prefix_aware(&shell_kind.prepend_command_prefix(&input_program))
                .context("shell quoting")?
        )?;
        for arg in input_args {
            let arg = shell_kind.try_quote(arg).context("shell quoting")?;
            write!(exec, " {arg}")?;
        }
    } else {
        // Launch an interactive shell session
        write!(exec, "{ssh_shell}")?;
    };

    let mut args = Vec::new();
    args.extend(ssh_options);

    if let Some((local_port, host, remote_port)) = port_forward {
        args.push("-L".into());
        args.push(format!(
            "{}:{}:{}",
            local_port,
            bracket_ipv6(&host),
            remote_port
        ));
    }

    // LogLevel=ERROR suppresses the "Connection to ... closed." message while
    // preserving SSH errors.
    args.extend(["-o".into(), "LogLevel=ERROR".into()]);
    match interactive {
        // -t forces pseudo-TTY allocation (for interactive use)
        Interactive::Yes => args.push("-t".into()),
        // -T disables pseudo-TTY allocation (for non-interactive piped stdio)
        Interactive::No => args.push("-T".into()),
    }

    // The destination must come after all options but before the command
    args.push(ssh_destination.into());

    // Windows OpenSSH server incorrectly escapes the command string when the PTY is used.
    // The simplest way to work around this is to use a base64 encoded command, which doesn't require escaping.
    let utf16_bytes: Vec<u16> = exec.encode_utf16().collect();
    let byte_slice: Vec<u8> = utf16_bytes.iter().flat_map(|&u| u.to_le_bytes()).collect();
    let base64_encoded = base64::engine::general_purpose::STANDARD.encode(&byte_slice);

    args.push(format!("powershell.exe -E {}", base64_encoded));

    Ok(CommandTemplate {
        program: "ssh".into(),
        args,
        env: ssh_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// ADR-F46: the agent sandbox helper is started with no environment of its
    /// own and an argv of hex digits, so whatever the login shell (csh here),
    /// nothing in the command line is shell syntax.
    #[test]
    fn test_build_command_runs_the_sandbox_helper_without_any_environment() -> Result<()> {
        let helper = ".zed_server/zed-remote-server-stable-1.17.2";
        let packed = "2d2d72656d6f74652d73616e64626f782d68656c706572.2d2d";
        let command = build_command_posix(
            Some(helper.to_string()),
            &[
                "--remote-sandbox-helper-argv-hex".to_string(),
                packed.to_string(),
            ],
            &HashMap::default(),
            None,
            None,
            HashMap::default(),
            PathStyle::Unix,
            "/bin/csh",
            ShellKind::Csh,
            Vec::new(),
            "user@host",
            Interactive::Yes,
        )?;
        let expected = format!("cd && exec env {helper} --remote-sandbox-helper-argv-hex {packed}");
        assert_eq!(command.args.last(), Some(&expected));
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn test_build_stdio_command() -> Result<()> {
        let mut connection = SshRemoteConnection {
            socket: SshSocket {
                connection_options: SshConnectionOptions {
                    host: SshConnectionHost::from("host"),
                    username: Some(String::from("user")),
                    port: Some(2222),
                    ..SshConnectionOptions::default()
                },
                socket_path: PathBuf::from("/tmp/zed-test.sock"),
                envs: HashMap::from_iter([(
                    String::from("SSH_ASKPASS_REQUIRE"),
                    String::from("force"),
                )]),
            },
            master_process: Mutex::new(None),
            killed: AtomicBool::new(false),
            remote_binary_path: None,
            ssh_platform: RemotePlatform {
                os: RemoteOs::Linux,
                arch: RemoteArch::X86_64,
            },
            ssh_os_version: None,
            ssh_path_style: PathStyle::Unix,
            ssh_shell: String::from("/bin/bash"),
            ssh_shell_kind: ShellKind::Posix,
            ssh_default_system_shell: String::from("/bin/sh"),
            ssh_home_dir: None,
            _temp_dir: TempDir::new()?,
        };
        let command = |env: HashMap<String, String>, working_dir: Option<&str>| RemoteCommand {
            program: String::from("agent"),
            args: vec![String::from("--token=argument-secret")],
            env,
            working_dir: working_dir.map(str::to_owned),
        };
        let valid_env =
            HashMap::from_iter([(String::from("API_KEY"), String::from("agent-secret"))]);
        assert_eq!(
            connection
                .build_stdio_command(command(valid_env.clone(), Some("~/project")))
                .unwrap_err()
                .to_string(),
            "Remote binary path not set"
        );

        connection.remote_binary_path = Some(Arc::from(RelPath::from_unix_str(
            ".local/share/zed/remote server",
        )?));
        for (home_dir, working_dir, shell_kind, expected_exec) in [
            (
                None,
                Some("~/project"),
                ShellKind::Posix,
                "cd \"$HOME\"/project && exec \"$HOME\"/'.local/share/zed/remote server' exec",
            ),
            (
                None,
                Some("/srv/project!"),
                ShellKind::Tcsh,
                "cd '/srv/project'\\! && exec \"$HOME\"/'.local/share/zed/remote server' exec",
            ),
            (
                Some("/home/user/"),
                Some("~/project"),
                ShellKind::Posix,
                "cd \"$HOME\"/project && exec '/home/user/.local/share/zed/remote server' exec",
            ),
            (
                Some("/home/bang!user"),
                Some("/srv/project!"),
                ShellKind::Tcsh,
                "cd '/srv/project'\\! && exec '/home/bang'\\!'user/.local/share/zed/remote server' exec",
            ),
            (
                Some("/home/user/"),
                Some("/srv/project 'x'"),
                ShellKind::Fish,
                "cd \"/srv/project 'x'\" && exec '/home/user/.local/share/zed/remote server' exec",
            ),
            (
                Some("/home/user/"),
                Some("/srv/project"),
                ShellKind::Nushell,
                "cd /srv/project ; exec '/home/user/.local/share/zed/remote server' exec",
            ),
            (
                None,
                Some("~/project 'x'"),
                ShellKind::Nushell,
                "cd ($env.HOME | path join \"project 'x'\") ; exec ($env.HOME | path join '.local/share/zed/remote server') exec",
            ),
            (
                None,
                Some("~/"),
                ShellKind::Nushell,
                "cd $env.HOME ; exec ($env.HOME | path join '.local/share/zed/remote server') exec",
            ),
            (
                Some("/home/a^b"),
                Some("/srv/O'Brien $x"),
                ShellKind::Nushell,
                "cd \"/srv/O'Brien $x\" ; exec '/home/a^b/.local/share/zed/remote server' exec",
            ),
            (
                Some("/home/user/"),
                None,
                ShellKind::Posix,
                "cd && exec '/home/user/.local/share/zed/remote server' exec",
            ),
        ] {
            connection.ssh_home_dir = home_dir.map(String::from);
            connection.ssh_shell_kind = shell_kind;
            let expected_payload = command(valid_env.clone(), None).encode()?;
            let mut env = valid_env.clone();
            env.insert(String::from("NAME\0NUL"), String::from("dropped"));
            let (template, payload) = connection.build_stdio_command(command(env, working_dir))?;
            assert_eq!(template.program, "ssh");
            assert_eq!(
                template.args,
                [
                    // ADR-F21: the keepalive this fork adds to every `ssh`.
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                    "-p",
                    "2222",
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "ControlPath=/tmp/zed-test.sock",
                    "-o",
                    "LogLevel=ERROR",
                    "-T",
                    "user@host",
                    expected_exec,
                ]
            );
            assert_eq!(template.env, connection.socket.envs);
            assert_eq!(payload, expected_payload);
        }

        connection.ssh_platform.os = RemoteOs::Windows;
        connection.ssh_path_style = PathStyle::Windows;
        let command = RemoteCommand {
            program: String::from("agent"),
            args: vec![String::from("--acp")],
            env: HashMap::from_iter([
                (String::from("API_KEY"), String::from("agent-secret")),
                (
                    String::from("HTTPS_PROXY"),
                    String::from("http://127.0.0.1:53123"),
                ),
            ]),
            working_dir: Some(String::from(r"C:\project")),
        };
        let expected = connection.build_command(
            Some(command.program.clone()),
            &command.args,
            &command.env,
            command.working_dir.clone(),
            None,
            Interactive::No,
        )?;
        let (template, payload) = connection.build_stdio_command(command)?;
        assert_eq!(template.program, expected.program);
        assert_eq!(template.args, expected.args);
        assert_eq!(template.env, expected.env);
        assert_eq!(payload, Vec::<u8>::new());
        // ADR-F14: the remote's egress proxy reaches the process; the rest of
        // its environment does not cross to a Windows remote at all.
        let decoded = {
            use base64::Engine as _;
            let encoded = template
                .args
                .last()
                .and_then(|arg| arg.strip_prefix("powershell.exe -E "))
                .context("missing the base64 encoded remote command")?;
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            String::from_utf16(
                &decoded
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect::<Vec<_>>(),
            )?
        };
        assert!(
            decoded.contains("$env:HTTPS_PROXY='http://127.0.0.1:53123'"),
            "{decoded}"
        );
        assert!(!decoded.contains("agent-secret"), "{decoded}");
        Ok(())
    }

    #[test]
    fn probe_returns_parseable_environment() -> Result<()> {
        #[cfg(windows)]
        {
            use base64::Engine as _;

            for powershell in [
                PathBuf::from("powershell.exe"),
                #[cfg(target_arch = "x86_64")]
                PathBuf::from(std::env::var("SystemRoot")?)
                    // The 32-bit PowerShell on 64-bit Windows
                    .join("SysWOW64/WindowsPowerShell/v1.0/powershell.exe"),
            ] {
                for language_mode in ["FullLanguage", "ConstrainedLanguage"] {
                    let script = format!(
                        "$ExecutionContext.SessionState.LanguageMode = '{language_mode}'; {}",
                        RemoteEnvironment::windows_script()
                    );
                    let encoded_script = base64::engine::general_purpose::STANDARD.encode(
                        script
                            .encode_utf16()
                            .flat_map(u16::to_le_bytes)
                            .collect::<Vec<_>>(),
                    );
                    let output = smol::block_on(
                        util::command::new_command(&powershell)
                            .args(["-E", &encoded_script])
                            .output(),
                    )?;
                    assert!(
                        output.status.success(),
                        "{} ({language_mode}): {}",
                        powershell.display(),
                        String::from_utf8_lossy(&output.stderr)
                    );

                    let environment =
                        RemoteEnvironment::parse_windows(&String::from_utf8_lossy(&output.stdout))?;
                    assert_eq!(environment.platform.os, RemoteOs::Windows);
                    assert_eq!(environment.platform.arch.as_str(), std::env::consts::ARCH);
                    assert!(!environment.shell.is_empty());
                    assert!(environment.os_version.is_some());
                }
            }
        }

        #[cfg(unix)]
        {
            let output = smol::block_on(
                util::command::new_command("/bin/sh")
                    .args(["-c", &RemoteEnvironment::posix_script()])
                    .env("SHELL", "/bin/sh")
                    .output(),
            )?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );

            let environment = RemoteEnvironment::parse_posix(std::str::from_utf8(&output.stdout)?)?;
            assert_eq!(environment.shell, "/bin/sh");
            assert!(environment.os_version.is_some());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn version_query_failure_does_not_abort_probe() -> Result<()> {
        let command = if cfg!(target_os = "macos") {
            "sw_vers"
        } else {
            "cat"
        };
        let script = format!(
            "{command}() {{ return 7; }}; {}",
            RemoteEnvironment::posix_script()
        );
        let output = smol::block_on(
            util::command::new_command("/bin/bash")
                .args(["--posix", "-e", "-c", &script])
                .output(),
        )?;
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let environment = RemoteEnvironment::parse_posix(std::str::from_utf8(&output.stdout)?)?;
        assert_eq!(environment.os_version, None);
        Ok(())
    }

    #[test]
    fn parses_posix_environment_output() -> Result<()> {
        for (shell, platform, version, expected_os, expected_arch, expected_version) in [
            (
                "/usr/bin/fish",
                "Linux x86_64",
                "NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_ID=\"24.04\"\n",
                RemoteOs::Linux,
                RemoteArch::X86_64,
                "ubuntu 24.04",
            ),
            (
                "/bin/zsh",
                "Darwin arm64",
                "15.6.1\n",
                RemoteOs::MacOs,
                RemoteArch::Aarch64,
                "15.6.1",
            ),
        ] {
            let output = [
                "Welcome\nShell startup output\n",
                shell,
                platform,
                version,
                "0",
            ]
            .join("\0");
            let environment = RemoteEnvironment::parse_posix(&output)?;

            assert_eq!(environment.shell, shell);
            assert_eq!(environment.platform.os, expected_os);
            assert_eq!(environment.platform.arch, expected_arch);
            assert_eq!(environment.os_version.as_deref(), Some(expected_version));
        }
        Ok(())
    }

    #[test]
    fn parses_windows_environment_output() -> Result<()> {
        for (shell, architecture, raw_version, expected_arch, expected_version) in [
            (
                "powershell.exe",
                "AMD64",
                "\r\nMicrosoft Windows [Version 10.0.19045.5011]\r\n",
                RemoteArch::X86_64,
                "10.0.19045",
            ),
            (
                "pwsh.exe",
                "ARM64",
                "\r\nMicrosoft Windows [Version 10.0.26100.2033]\r\n",
                RemoteArch::Aarch64,
                "10.0.26100",
            ),
        ] {
            let output = [
                "Shell startup output\r\n",
                shell,
                architecture,
                raw_version,
                "0",
            ]
            .join("\0");
            let environment = RemoteEnvironment::parse_windows(&output)?;

            assert_eq!(environment.shell, shell);
            assert_eq!(environment.platform.os, RemoteOs::Windows);
            assert_eq!(environment.platform.arch, expected_arch);
            assert_eq!(environment.os_version.as_deref(), Some(expected_version));
        }
        Ok(())
    }

    #[test]
    fn uses_default_shell_when_shell_is_empty() -> Result<()> {
        let output = [
            "",
            "",
            "Linux x86_64",
            "ID=ubuntu\nVERSION_ID=\"24.04\"\n",
            "0",
        ]
        .join("\0");
        assert_eq!(RemoteEnvironment::parse_posix(&output)?.shell, "sh");

        let output = [
            "",
            "",
            "AMD64",
            "Microsoft Windows [Version 10.0.19045.5011]",
            "0",
        ]
        .join("\0");
        assert_eq!(RemoteEnvironment::parse_windows(&output)?.shell, "cmd.exe");
        Ok(())
    }

    #[test]
    fn rejects_output_without_environment_fields() {
        assert!(RemoteEnvironment::parse_posix("shell startup output\n").is_err());
        assert!(RemoteEnvironment::parse_windows("shell startup output\r\n").is_err());
    }

    #[test]
    fn rejects_unsupported_windows_architecture() {
        let output = [
            "",
            "cmd.exe",
            "x86",
            "Microsoft Windows [Version 10.0.19045.5011]",
            "0",
        ]
        .join("\0");
        assert!(RemoteEnvironment::parse_windows(&output).is_err());
    }

    #[test]
    fn ignores_os_version_when_query_fails() -> Result<()> {
        for (environment, expected_shell, expected_os) in [
            (
                RemoteEnvironment::parse_posix(
                    &[
                        "",
                        "/bin/sh",
                        "Linux x86_64",
                        "ID=ubuntu\nVERSION_ID=\"24.04\"\n",
                        "1",
                    ]
                    .join("\0"),
                )?,
                "/bin/sh",
                RemoteOs::Linux,
            ),
            (
                RemoteEnvironment::parse_windows(
                    &[
                        "",
                        "powershell.exe",
                        "AMD64",
                        "Microsoft Windows [Version 10.0.19045.5011]",
                        "1",
                    ]
                    .join("\0"),
                )?,
                "powershell.exe",
                RemoteOs::Windows,
            ),
        ] {
            assert_eq!(environment.shell, expected_shell);
            assert_eq!(environment.platform.os, expected_os);
            assert_eq!(environment.platform.arch, RemoteArch::X86_64);
            assert_eq!(environment.os_version, None);
        }
        Ok(())
    }

    #[test]
    fn test_build_command() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("INPUT_VA".to_string(), "val".to_string());
        let mut env = HashMap::default();
        env.insert("SSH_VAR".to_string(), "ssh-val".to_string());

        // Test non-interactive command (interactive=false should use -T)
        let command = build_command_posix(
            Some("remote_program".to_string()),
            &["arg1".to_string(), "arg2".to_string()],
            &input_env,
            Some("~/work".to_string()),
            None,
            env.clone(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec!["-o".to_string(), "ControlMaster=auto".to_string()],
            "user@host",
            Interactive::No,
        )?;
        assert_eq!(command.program, "ssh");
        // Should contain -T for non-interactive
        assert!(command.args.iter().any(|arg| arg == "-T"));
        assert!(!command.args.iter().any(|arg| arg == "-t"));

        // Test interactive command (interactive=true should use -t)
        let command = build_command_posix(
            Some("remote_program".to_string()),
            &["arg1".to_string(), "arg2".to_string()],
            &input_env,
            Some("~/work".to_string()),
            None,
            env.clone(),
            PathStyle::Unix,
            "/bin/fish",
            ShellKind::Fish,
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
            Interactive::Yes,
        )?;

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-p",
                "2222",
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@host",
                "cd \"$HOME\"/work && exec env 'INPUT_VA=val' remote_program arg1 arg2"
            ]
        );
        assert_eq!(command.env, env);

        let mut input_env = HashMap::default();
        input_env.insert("INPUT_VA".to_string(), "val".to_string());
        let mut env = HashMap::default();
        env.insert("SSH_VAR".to_string(), "ssh-val".to_string());

        let command = build_command_posix(
            None,
            &[],
            &input_env,
            None,
            Some((1, "foo".to_owned(), 2)),
            env.clone(),
            PathStyle::Unix,
            "/bin/fish",
            ShellKind::Fish,
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
            Interactive::Yes,
        )?;

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-p",
                "2222",
                "-L",
                "1:foo:2",
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@host",
                "cd && exec env 'INPUT_VA=val' /bin/fish -l"
            ]
        );
        assert_eq!(command.env, env);

        Ok(())
    }

    #[test]
    fn test_build_command_quotes_env_assignment() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("ZED$(echo foo)".to_string(), "value".to_string());

        let command = build_command_posix(
            Some("remote_program".to_string()),
            &[],
            &input_env,
            None,
            None,
            HashMap::default(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        let remote_command = command
            .args
            .last()
            .context("missing remote command argument")?;
        assert!(
            remote_command.contains("exec env 'ZED$(echo foo)=value' remote_program"),
            "expected env assignment to be quoted, got: {remote_command}"
        );

        Ok(())
    }

    /// The 8K limit still drops the rest of the environment, but a Windows
    /// remote whose AI child processes were meant to run behind the egress
    /// proxy (ADR-F14) has to receive the variables that point at it.
    #[test]
    fn test_build_command_windows_forwards_only_the_proxy_env() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("HTTPS_PROXY".to_string(), "http://127.0.0.1:53123".into());
        input_env.insert("NO_PROXY".to_string(), "localhost,127.0.0.1,::1".into());
        input_env.insert("SOME_HUGE_VAR".to_string(), "x".repeat(9000));

        let command = build_command_windows(
            Some("remote_program".to_string()),
            &[],
            &input_env,
            None,
            None,
            HashMap::default(),
            PathStyle::Windows,
            "powershell",
            ShellKind::PowerShell,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        let encoded = command
            .args
            .last()
            .and_then(|arg| arg.strip_prefix("powershell.exe -E "))
            .context("missing the base64 encoded remote command")?;
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        let decoded = String::from_utf16(
            &decoded
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )?;

        assert_eq!(
            decoded,
            "$env:HTTPS_PROXY='http://127.0.0.1:53123' ; \
             $env:NO_PROXY='localhost,127.0.0.1,::1' ; &remote_program"
        );
        Ok(())
    }

    /// Every byte here is 2.7 on an 8K-limited command line, so a caller that
    /// hands over an oversized proxy value gets the pre-fork behaviour rather
    /// than a connection that fails to start.
    #[test]
    fn test_windows_forwarded_env_is_dropped_when_it_is_too_large() {
        let mut input_env = HashMap::default();
        input_env.insert(
            "NO_PROXY".to_string(),
            "a".repeat(WINDOWS_FORWARDED_ENV_MAX_BYTES),
        );
        assert_eq!(windows_forwarded_env_assignments(&input_env), None);

        input_env.insert("NO_PROXY".to_string(), "localhost".to_string());
        assert_eq!(
            windows_forwarded_env_assignments(&input_env).as_deref(),
            Some("$env:NO_PROXY='localhost' ; ")
        );
    }

    /// A bare `http://…` on the right of `=` is a command for PowerShell to
    /// run, so the value has to be quoted even though it holds nothing
    /// `try_quote` considers a metacharacter.
    #[test]
    fn test_windows_forwarded_env_quotes_every_value() {
        let mut input_env = HashMap::default();
        input_env.insert("HTTP_PROXY".to_string(), "http://127.0.0.1:1".to_string());
        input_env.insert("http_proxy".to_string(), "it's".to_string());

        assert_eq!(
            windows_forwarded_env_assignments(&input_env).as_deref(),
            Some("$env:HTTP_PROXY='http://127.0.0.1:1' ; $env:http_proxy='it''s' ; ")
        );
    }

    /// Verbatim output of [`posix_probe_script`] captured from the Ubuntu 24.04
    /// test host on 2026-09-05, with the leading blank line each section carries
    /// because the separator is echoed on a line of its own.
    const PROBE_OUTPUT_UBUNTU: &str = "/bin/bash\n--zed-probe--\nLinux x86_64\n--zed-probe--\n\
        PRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\n\
        --zed-probe--\nyes\n--zed-probe--\n586446160 1788531474\n--zed-probe--\nyes\n";

    #[test]
    fn test_posix_probe_script_is_a_single_line() {
        let script = posix_probe_script(
            ShellKind::Posix,
            ".zed_server/zed-remote-server-dev-build",
            ".zed_server/slirp4netns",
        )
        .expect("both paths are quotable");
        // `SshSocket::ssh_command` asserts on this: not every remote login shell
        // keeps an embedded newline intact.
        assert!(!script.contains('\n'));
        assert_eq!(
            script.matches(PROBE_SECTION_SEPARATOR).count(),
            6,
            "the seven sections `parse_posix_probe` expects need six separators"
        );
    }

    #[test]
    fn test_parse_posix_probe() {
        let probe = parse_posix_probe(
            PROBE_OUTPUT_UBUNTU,
            ".zed_server/zed-remote-server-dev-build",
        )
        .expect("the captured output is what the script prints");
        assert_eq!(probe.shell, "/bin/bash");
        assert_eq!(probe.platform.os, RemoteOs::Linux);
        assert_eq!(probe.platform.arch, RemoteArch::X86_64);
        assert_eq!(probe.os_version.as_deref(), Some("ubuntu 24.04"));
        assert!(probe.server_binary_runs);
        assert_eq!(probe.server_binary_stat, Some((586446160, 1788531474)));
        assert!(probe.slirp4netns_runs);
        assert_eq!(probe.home_dir, None);
    }

    #[test]
    fn test_parse_posix_probe_reads_the_home_directory() {
        let output = format!(
            "{PROBE_OUTPUT_UBUNTU}--zed-probe--\nZED_HOME_BEGIN\n/home/a --zed-probe-- b\nZED_HOME_END"
        );
        let probe =
            parse_posix_probe(&output, ".zed_server/zed-remote-server-dev-build").expect("parses");
        assert_eq!(probe.home_dir.as_deref(), Some("/home/a --zed-probe-- b"));
        assert!(probe.slirp4netns_runs);

        let output = format!("{PROBE_OUTPUT_UBUNTU}--zed-probe--\nrelative\n");
        let probe = parse_posix_probe(&output, ".zed_server/zed-remote-server-dev-build")
            .expect("a home directory that does not parse leaves the rest usable");
        assert_eq!(probe.home_dir, None);
    }

    #[test]
    fn test_parse_posix_probe_with_nothing_installed_on_the_remote() {
        let output = "/bin/sh\n--zed-probe--\nDarwin arm64\n--zed-probe--\n15.6.1\n\
                      --zed-probe--\n--zed-probe--\n--zed-probe--\n";
        let probe =
            parse_posix_probe(output, ".zed_server/zed-remote-server-dev-build").expect("parses");
        assert_eq!(probe.platform.os, RemoteOs::MacOs);
        assert_eq!(probe.os_version.as_deref(), Some("15.6.1"));
        assert!(!probe.server_binary_runs);
        assert_eq!(probe.server_binary_stat, None);
        assert!(!probe.slirp4netns_runs);
    }

    #[test]
    fn test_parse_posix_probe_rejects_output_that_is_not_the_script() {
        // What a Windows remote's shell answers: no separators at all.
        assert!(
            parse_posix_probe("'sh' is not recognized as an internal command\n", "x").is_none()
        );
        // A POSIX `sh` on a Windows remote (Git for Windows) does run the
        // script, and `uname` is what gives it away.
        let mingw = PROBE_OUTPUT_UBUNTU.replace("Linux x86_64", "MINGW64_NT-10.0-19045 x86_64");
        assert!(parse_posix_probe(&mingw, "x").is_none());
        // Truncated output (a connection that died mid-probe).
        assert!(parse_posix_probe("/bin/bash\n--zed-probe--\nLinux x86_64\n", "x").is_none());
    }

    #[test]
    fn test_server_binary_names() {
        assert_eq!(
            server_binary_names(ReleaseChannel::Dev, &Version::new(1, 17, 2), false),
            (
                "forgekairos-remote-server-dev-".to_string(),
                format!(
                    "forgekairos-remote-server-dev-build-jp{}",
                    rpc::proto::jp_enhance::JP_ENHANCE_WIRE_REVISION
                )
            )
        );
        assert_eq!(
            server_binary_names(ReleaseChannel::Stable, &Version::new(1, 17, 2), true),
            (
                "forgekairos-remote-server-stable-".to_string(),
                format!(
                    "forgekairos-remote-server-stable-1.17.2-jp{}.exe",
                    rpc::proto::jp_enhance::JP_ENHANCE_WIRE_REVISION
                )
            )
        );
    }

    #[test]
    fn test_stale_server_files_cleanup_script_posix() -> Result<()> {
        assert_eq!(
            stale_server_files_cleanup_script_posix(
                ".zed_server",
                "forgekairos-remote-server-dev-",
                &[],
                "forgekairos-remote-server-dev-build-jp2",
            )?,
            "for stale in .zed_server/forgekairos-remote-server-dev-* .zed_server/download-*; \
             do if [ -e \"$stale\" ] && [ \"$stale\" != .zed_server/forgekairos-remote-server-dev-build-jp2 ]; \
             then rm -f -- \"$stale\"; fi; done; exit 0"
        );
        assert_eq!(
            stale_server_files_cleanup_script_posix(
                ".zed_server",
                "forgekairos-remote-server-dev-",
                &["zed-remote-server-dev-".to_string()],
                "forgekairos-remote-server-dev-build-jp2",
            )?,
            "for stale in .zed_server/forgekairos-remote-server-dev-* .zed_server/download-*; \
             do if [ -e \"$stale\" ] && [ \"$stale\" != .zed_server/forgekairos-remote-server-dev-build-jp2 ]; \
             then rm -f -- \"$stale\"; fi; done; \
             for stale in .zed_server/zed-remote-server-dev-*-jp[0-9]*; \
             do if [ -e \"$stale\" ]; then case \"${stale##*-jp}\" in \
             ''|*[!0-9]*) ;; *) rm -f -- \"$stale\";; esac; fi; done; exit 0"
        );
        Ok(())
    }

    #[test]
    fn test_stale_server_files_cleanup_script_windows() -> Result<()> {
        assert_eq!(
            stale_server_files_cleanup_script_windows(
                ".zed_server",
                "forgekairos-remote-server-dev-",
                &["zed-remote-server-dev-".to_string()],
                "forgekairos-remote-server-dev-build-jp2.exe",
            )?,
            "Get-ChildItem -LiteralPath '.zed_server' -Force -ErrorAction SilentlyContinue | \
             Where-Object { (($_.Name -like 'forgekairos-remote-server-dev-*' -or $_.Name -like 'download-*') \
             -and $_.Name -ne 'forgekairos-remote-server-dev-build-jp2.exe') \
             -or (($_.Name -like 'zed-remote-server-dev-*') -and $_.Name -match '-jp[0-9]+(\\.exe)?\\z') } | \
             Remove-Item -Force -Recurse -ErrorAction SilentlyContinue; exit 0"
        );
        Ok(())
    }

    /// What a remote's `~/.zed_server` can hold when this build installs its
    /// stable 1.21.0 server: its own earlier versions, the other channels', the
    /// earlier fork names, upstream Zed's own server, and leftovers.
    const SERVER_DIRECTORY_BEFORE_THE_CLEANUP: &[&str] = &[
        "forgekairos-remote-server-stable-1.21.0-jp2",
        "forgekairos-remote-server-stable-1.20.0-jp2",
        "forgekairos-remote-server-dev-build-jp2",
        "zed-remote-server-stable-1.17.2-jp1",
        "zed-remote-server-stable-1.20.0-jp2",
        "zed-remote-server-stable-1.17.2-jpx",
        "zed-remote-server-dev-build-jp2",
        "zed-remote-server-stable-1.21.0",
        "zed-remote-server-preview-1.22.0-pre",
        "zed-remote-server-dev-build",
        "download-7-forgekairos-remote-server-linux-x86_64",
        "slirp4netns",
    ];

    /// What [`SERVER_DIRECTORY_BEFORE_THE_CLEANUP`] keeps: the binary being
    /// installed, every other channel's, upstream Zed's (it carries no
    /// `-jp<revision>`), and whatever is not a server binary.
    const SERVER_DIRECTORY_AFTER_THE_CLEANUP: &[&str] = &[
        "forgekairos-remote-server-dev-build-jp2",
        "forgekairos-remote-server-stable-1.21.0-jp2",
        "slirp4netns",
        "zed-remote-server-dev-build",
        "zed-remote-server-dev-build-jp2",
        "zed-remote-server-preview-1.22.0-pre",
        "zed-remote-server-stable-1.17.2-jpx",
        "zed-remote-server-stable-1.21.0",
    ];

    /// Runs `script` with `program` in a new directory holding a
    /// `.zed_server` with `names` in it, and returns the names left, sorted.
    /// `None` when `program` cannot be started on this machine.
    fn server_directory_after(
        program: &str,
        arguments: &[&str],
        script: &str,
        names: &[String],
    ) -> Option<Vec<String>> {
        let home = tempfile::tempdir().expect("tempdir");
        let directory = home.path().join(".zed_server");
        std::fs::create_dir(&directory).expect("create the server directory");
        for name in names {
            std::fs::write(directory.join(name), "binary").expect("write a server file");
        }
        let output = match smol::block_on(
            smol::process::Command::new(program)
                .args(arguments)
                .arg(script)
                .current_dir(home.path())
                .output(),
        ) {
            Ok(output) => output,
            Err(error) => {
                eprintln!("not running the cleanup script, {program} cannot be started: {error}");
                return None;
            }
        };
        assert!(
            output.status.success(),
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut left = std::fs::read_dir(&directory)
            .expect("list the server directory")
            .map(|entry| {
                entry
                    .expect("a directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        left.sort();
        Some(left)
    }

    /// H1: the cleanup removes this fork's earlier binaries of the channel it
    /// installs, under the current name and the earlier one, and never
    /// upstream Zed's server, which shares the directory and the earlier
    /// prefix. Run by a real `sh` (Git for Windows' on Windows) rather than
    /// only compared as a string.
    #[test]
    fn test_stale_server_files_cleanup_keeps_upstream_servers_under_sh() -> Result<()> {
        let (prefix, _) =
            server_binary_names(ReleaseChannel::Stable, &Version::new(1, 21, 0), false);
        let script = stale_server_files_cleanup_script_posix(
            ".zed_server",
            &prefix,
            &legacy_server_binary_name_prefixes(ReleaseChannel::Stable),
            "forgekairos-remote-server-stable-1.21.0-jp2",
        )?;
        let names = SERVER_DIRECTORY_BEFORE_THE_CLEANUP
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let Some(left) = server_directory_after("sh", &["-c"], &script, &names) else {
            #[cfg(unix)]
            panic!("sh must be available on a Unix host");
            #[cfg(not(unix))]
            return Ok(());
        };
        assert_eq!(left, SERVER_DIRECTORY_AFTER_THE_CLEANUP);
        Ok(())
    }

    /// The PowerShell cleanup a Windows remote runs, with the `.exe` names a
    /// Windows remote holds.
    #[cfg(windows)]
    #[test]
    fn test_stale_server_files_cleanup_keeps_upstream_servers_under_powershell() -> Result<()> {
        let (prefix, _) =
            server_binary_names(ReleaseChannel::Stable, &Version::new(1, 21, 0), true);
        let script = stale_server_files_cleanup_script_windows(
            ".zed_server",
            &prefix,
            &legacy_server_binary_name_prefixes(ReleaseChannel::Stable),
            "forgekairos-remote-server-stable-1.21.0-jp2.exe",
        )?;
        let with_exe = |name: &str| {
            if name.contains("remote-server") && !name.starts_with("download-") {
                format!("{name}.exe")
            } else {
                name.to_string()
            }
        };
        let names = SERVER_DIRECTORY_BEFORE_THE_CLEANUP
            .iter()
            .map(|name| with_exe(name))
            .collect::<Vec<_>>();
        let left = server_directory_after(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command"],
            &script,
            &names,
        )
        .expect("powershell is available on Windows");
        let mut expected = SERVER_DIRECTORY_AFTER_THE_CLEANUP
            .iter()
            .map(|name| with_exe(name))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(left, expected);
        Ok(())
    }

    /// When the remote is too full, the other channels' binaries make room,
    /// but only this build's: upstream Zed's and the earlier fork names stay.
    #[test]
    fn test_other_channel_sweep_keeps_upstream_servers_under_sh() -> Result<()> {
        let script = other_channel_server_binaries_cleanup_script_posix(
            ".zed_server",
            &server_binary_name_prefix(ReleaseChannel::Stable),
        )?;
        let names = SERVER_DIRECTORY_BEFORE_THE_CLEANUP
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let Some(left) = server_directory_after("sh", &["-c"], &script, &names) else {
            #[cfg(unix)]
            panic!("sh must be available on a Unix host");
            #[cfg(not(unix))]
            return Ok(());
        };
        let mut expected = SERVER_DIRECTORY_BEFORE_THE_CLEANUP
            .iter()
            .filter(|name| **name != "forgekairos-remote-server-dev-build-jp2")
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(left, expected);
        Ok(())
    }

    /// Names at the edges of the `-jp<digits>` rule, names a shell could
    /// misread, and entries that are not plain files, run through `sh` the way
    /// the remote's login shell receives the script: quoted by
    /// [`SshRemoteConnection::ssh_command`]'s rules, after a `cd` to `$HOME`.
    #[test]
    fn test_stale_server_files_cleanup_at_the_edges_under_sh() -> Result<()> {
        let removed = [
            "zed-remote-server-stable-1.20.0-jp2",
            "zed-remote-server-stable-1.20.0-jp12",
            "zed-remote-server-stable-1.20.0-jp2-jp3",
            "zed-remote-server-stable-a b-jp2",
            "zed-remote-server-stable-$(touch pwned)-jp2",
            "zed-remote-server-stable-x;touch pwned;-jp2",
            "zed-remote-server-stable-`touch pwned`-jp2",
            "zed-remote-server-stable-*-jp2",
            "zed-remote-server-stable-x\n-jp2",
            "forgekairos-remote-server-stable-1.20.0-jp2",
            "forgekairos-remote-server-stable-1.20.0",
            "download-1-zed-remote-server-stable-1.21.0",
        ];
        let kept = [
            "forgekairos-remote-server-stable-1.21.0-jp2",
            "zed-remote-server-stable-1.21.0",
            "zed-remote-server-stable-1.21.0.exe",
            "zed-remote-server-stable-1.21.0-jp",
            "zed-remote-server-stable-1.21.0-jp2x",
            "zed-remote-server-stable-1.21.0-jp2.gz",
            "zed-remote-server-stable-1.21.0-jp2.exe",
            "zed-remote-server-stable-1.21.0-jp2-foo",
            "zed-remote-server-stable-1.21.0-JP2",
            "zed-remote-server-stablex-1.21.0-jp2",
            "zed-remote-server-preview-1.22.0-jp2",
            "zed-remote-server-dev-build-jp2",
            "forgekairos-remote-server-dev-build-jp2",
            "zed-remote-server-stable-1.21.0-jp2 ",
        ];
        let script = stale_server_files_cleanup_script_posix(
            ".zed_server",
            &server_binary_name_prefix(ReleaseChannel::Stable),
            &legacy_server_binary_name_prefixes(ReleaseChannel::Stable),
            "forgekairos-remote-server-stable-1.21.0-jp2",
        )?;
        let shell_kind = ShellKind::Posix;
        let mut command = shell_kind
            .try_quote_prefix_aware(&shell_kind.prepend_command_prefix("sh"))
            .context("quoting")?
            .into_owned();
        for argument in shell_kind.args_for_shell(false, script) {
            command.push(' ');
            command.push_str(&shell_kind.try_quote(&argument).context("quoting")?);
        }
        let command = format!("cd{} {command}", shell_kind.sequential_commands_separator());

        let home = tempfile::tempdir()?;
        let directory = home.path().join(".zed_server");
        std::fs::create_dir(&directory)?;
        let mut names = removed.iter().chain(&kept).copied().collect::<Vec<_>>();
        if cfg!(windows) {
            // Not names a Windows file system can hold.
            names.retain(|name| !name.contains(['*', '\n', '`']) && !name.ends_with(' '));
        }
        for name in &names {
            std::fs::write(directory.join(name), "binary")?;
        }
        #[cfg(unix)]
        {
            // A link under a name of this fork's is removed, never what it
            // points at; a directory under one is not a binary and stays.
            std::os::unix::fs::symlink(
                "zed-remote-server-stable-1.21.0",
                directory.join("zed-remote-server-stable-1.19.0-jp2"),
            )?;
            std::fs::create_dir(directory.join("zed-remote-server-stable-1.18.0-jp2"))?;
            std::fs::write(
                directory
                    .join("zed-remote-server-stable-1.18.0-jp2")
                    .join("inside"),
                "kept",
            )?;
        }

        let output = match smol::block_on(
            smol::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .env("HOME", home.path())
                .current_dir(home.path())
                .output(),
        ) {
            Ok(output) => output,
            Err(error) => {
                #[cfg(unix)]
                panic!("sh must be available on a Unix host: {error}");
                #[cfg(not(unix))]
                {
                    eprintln!("not running the cleanup script, sh cannot be started: {error}");
                    return Ok(());
                }
            }
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!home.path().join("pwned").exists());
        assert!(!directory.join("pwned").exists());
        for name in &names {
            assert_eq!(
                std::fs::symlink_metadata(directory.join(name)).is_ok(),
                kept.contains(name),
                "{name:?}"
            );
        }
        #[cfg(unix)]
        {
            assert!(
                std::fs::symlink_metadata(directory.join("zed-remote-server-stable-1.19.0-jp2"))
                    .is_err()
            );
            assert!(directory.join("zed-remote-server-stable-1.21.0").exists());
            assert!(
                directory
                    .join("zed-remote-server-stable-1.18.0-jp2")
                    .join("inside")
                    .exists()
            );
        }
        Ok(())
    }

    /// The PowerShell counterpart of
    /// [`test_stale_server_files_cleanup_at_the_edges_under_sh`], with the
    /// names a Windows file system can hold.
    #[cfg(windows)]
    #[test]
    fn test_stale_server_files_cleanup_at_the_edges_under_powershell() -> Result<()> {
        let removed = [
            "zed-remote-server-stable-1.20.0-jp2.exe",
            "zed-remote-server-stable-1.20.0-jp2",
            "zed-remote-server-stable-1.20.0-jp12.exe",
            "zed-remote-server-stable-a b-jp2.exe",
            "zed-remote-server-stable-$(New-Item pwned)-jp2.exe",
            "zed-remote-server-stable-x;New-Item pwned;-jp2.exe",
            "forgekairos-remote-server-stable-1.20.0-jp2.exe",
            "download-1-zed-remote-server-stable-1.21.0",
        ];
        let kept = [
            "forgekairos-remote-server-stable-1.21.0-jp2.exe",
            "zed-remote-server-stable-1.21.0.exe",
            "zed-remote-server-stable-1.21.0",
            "zed-remote-server-stable-1.21.0-jp.exe",
            "zed-remote-server-stable-1.21.0-jp2x.exe",
            "zed-remote-server-stable-1.21.0-jp2.gz",
            "zed-remote-server-stable-1.21.0-jp2.exe.gz",
            "zed-remote-server-stable-1.21.0-jp2-foo.exe",
            "zed-remote-server-stablex-1.21.0-jp2.exe",
            "zed-remote-server-preview-1.22.0-jp2.exe",
            "zed-remote-server-dev-build-jp2.exe",
            "forgekairos-remote-server-dev-build-jp2.exe",
        ];
        let script = stale_server_files_cleanup_script_windows(
            ".zed_server",
            &server_binary_name_prefix(ReleaseChannel::Stable),
            &legacy_server_binary_name_prefixes(ReleaseChannel::Stable),
            "forgekairos-remote-server-stable-1.21.0-jp2.exe",
        )?;
        let names = removed
            .iter()
            .chain(&kept)
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let left = server_directory_after(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command"],
            &script,
            &names,
        )
        .expect("powershell is available on Windows");
        let mut expected = kept.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(left, expected);
        Ok(())
    }

    /// The glob halves are interpolated unquoted, so a fragment that could
    /// carry a wildcard (or worse) has to stop the cleanup rather than widen
    /// what it deletes.
    #[test]
    fn test_cleanup_scripts_refuse_a_non_inert_glob() {
        for (dir, prefix) in [
            (".zed_server", "zed-remote-server-*-"),
            (".zed_server; rm -rf ~", "zed-remote-server-dev-"),
            (".zed_server", "zed remote server"),
            (".zed_server", ""),
        ] {
            assert!(
                stale_server_files_cleanup_script_posix(dir, prefix, &[], "keep").is_err(),
                "{dir:?} / {prefix:?} should be refused"
            );
            assert!(
                stale_server_files_cleanup_script_windows(dir, prefix, &[], "keep").is_err(),
                "{dir:?} / {prefix:?} should be refused"
            );
            let legacy = [prefix.to_string()];
            assert!(
                stale_server_files_cleanup_script_posix(dir, "ok-", &legacy, "keep").is_err(),
                "{dir:?} / legacy {prefix:?} should be refused"
            );
            assert!(
                stale_server_files_cleanup_script_windows(dir, "ok-", &legacy, "keep").is_err(),
                "{dir:?} / legacy {prefix:?} should be refused"
            );
            assert!(
                other_channel_server_binaries_cleanup_script_posix(dir, prefix).is_err(),
                "{dir:?} / {prefix:?} should be refused"
            );
            assert!(
                other_channel_server_binaries_cleanup_script_windows(dir, prefix).is_err(),
                "{dir:?} / {prefix:?} should be refused"
            );
        }
    }

    #[test]
    fn test_other_channel_server_binaries_cleanup_script_posix() -> Result<()> {
        assert_eq!(
            other_channel_server_binaries_cleanup_script_posix(
                ".zed_server",
                "forgekairos-remote-server-dev-",
            )?,
            "for stale in .zed_server/forgekairos-remote-server-*; \
             do if [ -e \"$stale\" ]; then \
             case \"${stale##*/}\" in forgekairos-remote-server-dev-*) ;; \
             *) if rm -f -- \"$stale\"; then echo \"${stale##*/}\"; fi;; esac; fi; \
             done; exit 0"
        );
        Ok(())
    }

    #[test]
    fn test_other_channel_server_binaries_cleanup_script_windows() -> Result<()> {
        assert_eq!(
            other_channel_server_binaries_cleanup_script_windows(
                ".zed_server",
                "forgekairos-remote-server-stable-",
            )?,
            "Get-ChildItem -LiteralPath '.zed_server' -Force -ErrorAction SilentlyContinue | \
             Where-Object { $_.Name -like 'forgekairos-remote-server-*' \
             -and -not ($_.Name -like 'forgekairos-remote-server-stable-*') } | \
             ForEach-Object { try { Remove-Item -LiteralPath $_.FullName -Force -Recurse \
             -ErrorAction Stop; $_.Name } catch { } }; exit 0"
        );
        Ok(())
    }

    /// The sweep has to leave this channel's binary — the one about to be
    /// installed, or already installed and running — where it is, whichever
    /// channel is asking.
    #[test]
    fn test_other_channel_sweep_keeps_its_own_channel() -> Result<()> {
        for channel in [ReleaseChannel::Dev, ReleaseChannel::Stable] {
            let prefix = server_binary_name_prefix(channel);
            let script =
                other_channel_server_binaries_cleanup_script_posix(".zed_server", &prefix)?;
            assert!(
                script.contains(&format!("in {prefix}*) ;;")),
                "{channel:?}: {script}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_parse_df_free_space() {
        assert_eq!(
            parse_df_free_space(
                "Filesystem      Size  Used Avail Use% Mounted on\n\
                 /dev/root        31G   30G  512M  99% /\n"
            )
            .as_deref(),
            Some("512M free, 99% used")
        );
        // A mount point with a space in it still parses: only the columns
        // before it are read.
        assert_eq!(
            parse_df_free_space(
                "Filesystem Size Used Avail Use% Mounted on\n\
                 /dev/sdb1 100G 1G 99G 1% /mnt/my disk\n"
            )
            .as_deref(),
            Some("99G free, 1% used")
        );
        // Header only, no output, and non-`df` noise all mean "cannot tell".
        assert_eq!(
            parse_df_free_space("Filesystem Size Used Avail Use% Mounted on\n"),
            None
        );
        assert_eq!(parse_df_free_space(""), None);
        assert_eq!(parse_df_free_space("df: '.zed_server': no such file"), None);
    }

    #[test]
    fn test_parse_df_available_kilobytes() {
        assert_eq!(
            parse_df_available_kilobytes(
                "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                 /dev/root         32380000 30800000   1580000      96% /\n"
            ),
            Some(1_580_000)
        );
        // A mount point with a space in it still parses.
        assert_eq!(
            parse_df_available_kilobytes(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                 /dev/sdb1 104857600 1048576 103809024 1% /mnt/my disk\n"
            ),
            Some(103_809_024)
        );
        // A human-readable figure is not a number of blocks and must not be
        // read as one.
        assert_eq!(
            parse_df_available_kilobytes(
                "Filesystem Size Used Avail Use% Mounted on\n\
                 /dev/root 31G 30G 512M 99% /\n"
            ),
            None
        );
        assert_eq!(parse_df_available_kilobytes(""), None);
        assert_eq!(
            parse_df_available_kilobytes("df: '.zed_server': no such file"),
            None
        );
    }

    /// Refusing before the transfer is the point: the same shortage found
    /// afterwards costs the whole upload and reports itself as
    /// `gunzip: write error`.
    #[test]
    fn test_insufficient_free_space_message() {
        // Unpacking needs the archive's room again, so exactly twice is the
        // smallest that works.
        assert_eq!(
            insufficient_free_space_message(50 * 1024 * 1024, 100 * 1024 * 1024),
            None
        );
        let message = insufficient_free_space_message(50 * 1024 * 1024, 60 * 1024 * 1024)
            .expect("60 MB cannot hold a 50 MB archive and what it unpacks to");
        assert!(
            message.contains("does not have enough free disk space"),
            "{message}"
        );
        assert!(message.contains("about 100 MB needed"), "{message}");
        assert!(message.contains("60 MB free"), "{message}");
        // Nothing readable still refuses rather than dividing to zero.
        assert!(insufficient_free_space_message(1, 0).is_some());
    }

    /// A `df` on `~/.zed_server` fails before the first install, which is the
    /// connection whose free space matters most.
    #[test]
    fn test_free_space_probe_falls_back_to_the_home_directory() {
        let targets = free_space_probe_targets(PathStyle::Unix);
        assert_eq!(targets[0], ".zed_server");
        assert_eq!(targets[1], ".");

        let script = windows_free_space_script(PathStyle::Windows, "$free");
        assert!(
            script.contains("Test-Path -LiteralPath $target"),
            "{script}"
        );
        assert!(script.contains("$target = $HOME"), "{script}");
    }

    #[test]
    fn test_sftp_put_command_quotes_paths() {
        assert_eq!(
            sftp_put_command(
                "/tmp/Zed Repro/remote_server",
                ".zed_server/downloaded server",
            ),
            "put \"/tmp/Zed Repro/remote_server\" \".zed_server/downloaded server\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_escapes_quotes_in_paths() {
        assert_eq!(
            sftp_put_command(
                r#"/tmp/Zed "Nightly"/remote_server"#,
                ".zed_server/remote_server",
            ),
            "put \"/tmp/Zed \\\"Nightly\\\"/remote_server\" \".zed_server/remote_server\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_doubles_trailing_destination_backslash_before_closing_quote() {
        assert_eq!(
            sftp_put_command("/tmp/remote_server", r"C:\zed server\"),
            "put \"/tmp/remote_server\" \"C:\\\\zed server\\\\\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_doubles_source_backslashes_for_posix_glob() {
        assert_eq!(
            sftp_put_command(
                r"/tmp/zed\server/remote_server",
                ".zed_server/remote_server",
            ),
            "put \"/tmp/zed\\\\server/remote_server\" \".zed_server/remote_server\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_doubles_windows_source_backslashes_on_all_platforms() {
        assert_eq!(
            sftp_put_command(
                r"C:\Users\Smit\Zed Repro\remote_server",
                ".zed_server/remote_server",
            ),
            "put \"C:\\\\Users\\\\Smit\\\\Zed Repro\\\\remote_server\" \".zed_server/remote_server\"\n"
        );
    }

    #[test]
    fn additional_args_add_keepalive_options() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            ..Default::default()
        };

        let args = options.additional_args();
        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                "ServerAliveInterval=15".to_string(),
                "-o".to_string(),
                "ServerAliveCountMax=3".to_string(),
            ]
        );

        assert!(
            options.additional_args_for_scp().is_empty(),
            "scp args should not gain keepalive options"
        );
    }

    #[test]
    fn additional_args_preserve_user_keepalive_options() {
        for user_arg in [
            "ServerAliveInterval=60".to_string(),
            "-oserveraliveinterval=60".to_string(),
        ] {
            let options = SshConnectionOptions {
                host: "example.com".into(),
                args: Some(vec![user_arg.clone()]),
                ..Default::default()
            };

            let args = options.additional_args();
            assert_eq!(
                args,
                vec![user_arg.clone()],
                "expected the user's own keepalive option to win: {args:?}"
            );
        }
    }

    #[test]
    fn scp_args_exclude_port_forward_flags() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            args: Some(vec![
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
            ]),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("127.0.0.1".to_string()),
                local_port: 8080,
                remote_host: Some("127.0.0.1".to_string()),
                remote_port: 80,
            }]),
            ..Default::default()
        };

        let ssh_args = options.additional_args();
        assert!(
            ssh_args.iter().any(|arg| arg.starts_with("-L")),
            "expected ssh args to include port-forward: {ssh_args:?}"
        );

        let scp_args = options.additional_args_for_scp();
        assert_eq!(
            scp_args,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
            ]
        );
    }

    #[test]
    fn test_host_parsing() -> Result<()> {
        let opts = SshConnectionOptions::parse_command_line("user@2001:db8::1")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("user@[2001:db8::1]:2222")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@[2001:db8::1]")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("2001:db8::1")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, None);
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("[2001:db8::1]:2222")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, None);
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@example.com:2222")?;
        assert_eq!(opts.host, "example.com".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@192.168.1.1:2222")?;
        assert_eq!(opts.host, "192.168.1.1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        Ok(())
    }

    #[test]
    fn test_parse_port_forward_spec_ipv6() -> Result<()> {
        let pf = parse_port_forward_spec("[::1]:8080:[::1]:80")?;
        assert_eq!(pf.local_host, Some("::1".to_string()));
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("::1".to_string()));
        assert_eq!(pf.remote_port, 80);

        let pf = parse_port_forward_spec("8080:[::1]:80")?;
        assert_eq!(pf.local_host, None);
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("::1".to_string()));
        assert_eq!(pf.remote_port, 80);

        let pf = parse_port_forward_spec("[2001:db8::1]:3000:[fe80::1]:4000")?;
        assert_eq!(pf.local_host, Some("2001:db8::1".to_string()));
        assert_eq!(pf.local_port, 3000);
        assert_eq!(pf.remote_host, Some("fe80::1".to_string()));
        assert_eq!(pf.remote_port, 4000);

        let pf = parse_port_forward_spec("127.0.0.1:8080:localhost:80")?;
        assert_eq!(pf.local_host, Some("127.0.0.1".to_string()));
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("localhost".to_string()));
        assert_eq!(pf.remote_port, 80);

        Ok(())
    }

    #[test]
    fn test_port_forward_ipv6_formatting() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("::1".to_string()),
                local_port: 8080,
                remote_host: Some("::1".to_string()),
                remote_port: 80,
            }]),
            ..Default::default()
        };

        let args = options.additional_args();
        assert!(
            args.iter().any(|arg| arg == "-L[::1]:8080:[::1]:80"),
            "expected bracketed IPv6 in -L flag: {args:?}"
        );
    }

    #[test]
    fn test_build_command_with_ipv6_port_forward() -> Result<()> {
        let command = build_command_posix(
            None,
            &[],
            &HashMap::default(),
            None,
            Some((8080, "::1".to_owned(), 80)),
            HashMap::default(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        assert!(
            command.args.iter().any(|arg| arg == "8080:[::1]:80"),
            "expected bracketed IPv6 in port forward arg: {:?}",
            command.args
        );

        Ok(())
    }
}
