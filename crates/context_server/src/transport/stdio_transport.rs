use std::path::PathBuf;
use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::io::{BufReader, BufWriter};
use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, Stream, StreamExt as _,
};
use gpui::AsyncApp;

use util::process::Child;
use util::shell::Shell;
use util::shell_builder::ShellBuilder;
use util::{ResultExt as _, TryFutureExt as _};

use crate::client::ModelContextServerBinary;
use crate::transport::Transport;

pub struct StdioTransport {
    stdout_sender: async_channel::Sender<String>,
    stdin_receiver: async_channel::Receiver<String>,
    stderr_receiver: async_channel::Receiver<String>,
    server: Child,
}

impl StdioTransport {
    pub fn new(
        binary: ModelContextServerBinary,
        working_directory: &Option<PathBuf>,
        cx: &AsyncApp,
    ) -> Result<Self> {
        let builder = ShellBuilder::new(&Shell::System, cfg!(windows)).non_interactive();
        let mut command =
            builder.build_std_command(Some(binary.executable.display().to_string()), &binary.args);

        command.envs(binary.env.unwrap_or_default());

        if let Some(working_directory) = working_directory {
            command.current_dir(working_directory);
        }

        let mut server = Child::spawn(
            command,
            std::process::Stdio::piped(),
            std::process::Stdio::piped(),
            std::process::Stdio::piped(),
        )?;

        let stdin = server.stdin.take().unwrap();
        let stdout = server.stdout.take().unwrap();
        let stderr = server.stderr.take().unwrap();

        let (stdin_sender, stdin_receiver) = async_channel::unbounded::<String>();
        let (stdout_sender, stdout_receiver) = async_channel::unbounded::<String>();
        let (stderr_sender, stderr_receiver) = async_channel::unbounded::<String>();

        cx.spawn(async move |_| Self::handle_output(stdin, stdout_receiver).log_err().await)
            .detach();

        cx.spawn(async move |_| Self::handle_input(stdout, stdin_sender).await)
            .detach();

        cx.spawn(async move |_| Self::handle_err(stderr, stderr_sender).await)
            .detach();

        Ok(Self {
            stdout_sender,
            stdin_receiver,
            stderr_receiver,
            server,
        })
    }

    async fn handle_input<Stdout>(stdin: Stdout, inbound_rx: async_channel::Sender<String>)
    where
        Stdout: AsyncRead + Unpin + Send + 'static,
    {
        let mut stdin = BufReader::new(stdin);
        let mut line = String::new();
        while let Ok(n) = stdin.read_line(&mut line).await {
            if n == 0 {
                break;
            }
            if inbound_rx.send(line.clone()).await.is_err() {
                break;
            }
            line.clear();
        }
    }

    async fn handle_output<Stdin>(
        stdin: Stdin,
        outbound_rx: async_channel::Receiver<String>,
    ) -> Result<()>
    where
        Stdin: AsyncWrite + Unpin + Send + 'static,
    {
        let mut stdin = BufWriter::new(stdin);
        let mut pinned_rx = Box::pin(outbound_rx);
        while let Some(message) = pinned_rx.next().await {
            log::trace!("outgoing message: {}", message);

            stdin.write_all(message.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }
        Ok(())
    }

    async fn handle_err<Stderr>(stderr: Stderr, stderr_tx: async_channel::Sender<String>)
    where
        Stderr: AsyncRead + Unpin + Send + 'static,
    {
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        while let Ok(n) = stderr.read_line(&mut line).await {
            if n == 0 {
                break;
            }
            if stderr_tx.send(line.clone()).await.is_err() {
                break;
            }
            line.clear();
        }
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&self, message: String) -> Result<()> {
        Ok(self.stdout_sender.send(message).await?)
    }

    fn receive(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(self.stdin_receiver.clone())
    }

    fn receive_err(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(self.stderr_receiver.clone())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.server.kill().log_err();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{ContextServer, ContextServerCommand, ContextServerId};
    use gpui::TestAppContext;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Arguments for `powershell.exe` that make a server which never answers:
    /// it writes its own pid to `pid_file` and sleeps. Started through the
    /// system shell like any stdio server, it runs as a grandchild of the
    /// spawned process, and when the system shell is the Microsoft Store
    /// build of PowerShell, outside the job the spawned process is in.
    fn hanging_server_args(pid_file: &Path) -> Vec<String> {
        vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            format!(
                "Set-Content -LiteralPath '{}' -Value ([System.Diagnostics.Process]::GetCurrentProcess().Id); Start-Sleep -Seconds 600",
                pid_file.display()
            ),
        ]
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut exit_code = 0u32;
            let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                && exit_code == STILL_ACTIVE.0 as u32;
            CloseHandle(handle).expect("failed to close process handle");
            alive
        }
    }

    fn wait_for_server_pid(pid_file: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(contents) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                assert!(process_is_alive(pid), "server should be running");
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the server to start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn assert_process_exits(pid: u32) {
        // `TerminateProcess` returns before the process is gone.
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_is_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "server (pid {pid}) outlived its transport"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[gpui::test]
    async fn test_dropping_the_transport_terminates_the_server(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("server_pid");
        let transport = StdioTransport::new(
            ModelContextServerBinary {
                executable: "powershell.exe".into(),
                args: hanging_server_args(&pid_file),
                env: None,
                timeout: None,
            },
            &None,
            &cx.to_async(),
        )
        .unwrap();
        let server_pid = wait_for_server_pid(&pid_file);

        drop(transport);

        assert_process_exits(server_pid);
    }

    #[gpui::test]
    async fn test_start_timeout_terminates_the_server(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("server_pid");
        let server = ContextServer::stdio(
            ContextServerId("hanging".into()),
            ContextServerCommand {
                path: "powershell.exe".into(),
                args: hanging_server_args(&pid_file),
                env: None,
                timeout: Some(5),
            },
            None,
        );
        let start = cx.spawn(|cx| async move { server.start(&cx).await });
        cx.run_until_parked();
        let server_pid = wait_for_server_pid(&pid_file);

        cx.executor().advance_clock(Duration::from_secs(6));
        let error = start
            .await
            .expect_err("a server that never answers should time out");
        assert!(
            error.to_string().contains("timeout"),
            "unexpected error: {error:#}"
        );
        // Lets the cancelled I/O tasks drop their handles on the transport.
        cx.run_until_parked();

        assert_process_exits(server_pid);
    }
}
