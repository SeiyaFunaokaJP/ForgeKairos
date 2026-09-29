//! ADR-F28: running the file transfer screen's commands on the *remote SSH
//! host* instead of on the machine Zed itself runs on.
//!
//! When a project is opened over SSH, the terminals, the worktrees and the
//! paths the user sees all belong to the remote host, so the transfer screen's
//! "local" pane has to be able to be that host too — otherwise `/home/ubuntu`
//! is looked up on the Windows IDE and comes back missing.
//!
//! A fresh `ssh` per operation was rejected: Windows has no ControlMaster, so
//! every listing would re-authenticate. These calls ride the already
//! multiplexed `remote_server` channel instead, which also means the host's
//! askpass prompts have to be relayed back here to be shown — the same shape
//! as the git askpass relay, keyed by transfer command rather than repository.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use client::{TypedEnvelope, proto};
use collections::HashMap;
use futures::FutureExt as _;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Task};
use parking_lot::Mutex;
use rpc::{AnyProtoClient, proto::REMOTE_SERVER_PROJECT_ID};
use util::ResultExt as _;

use crate::Project;

/// The transfer programs the remote host is willing to start (ADR-F28). An
/// enum rather than a free string, so a caller cannot ask the host for
/// arbitrary remote execution and find out only from its refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferProgram {
    Sftp,
    Scp,
    Curl,
}

impl TransferProgram {
    pub fn name(self) -> &'static str {
        match self {
            TransferProgram::Sftp => "sftp",
            TransferProgram::Scp => "scp",
            TransferProgram::Curl => "curl",
        }
    }
}

pub struct TransferCommand {
    pub program: TransferProgram,
    pub args: Vec<String>,
    /// Written to the process's stdin and then closed: the `sftp -b -` script,
    /// or curl's `--config -` file.
    pub stdin: Option<String>,
    pub working_directory: Option<String>,
    /// Whether OpenSSH's prompts should be relayed back to `askpass`.
    pub wants_askpass: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TransferCommandOutput {
    pub stdout: String,
    pub stderr: String,
    /// Absent when the process was terminated by a signal or cancelled.
    pub exit_code: Option<i32>,
    pub cancelled: bool,
}

impl TransferCommandOutput {
    pub fn succeeded(&self) -> bool {
        !self.cancelled && self.exit_code == Some(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteEntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Clone, Debug)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    pub kind: RemoteEntryKind,
    pub size: Option<u64>,
    pub mtime_epoch_seconds: Option<i64>,
}

pub enum RemoteFileOperation {
    MakeDirectory { path: String },
    Rename { from: String, to: String },
    Remove { path: String, recursive: bool },
}

/// Answers one of the remote host's askpass prompts, or cancels it with
/// `None`. Called on the foreground, since it puts a prompt on screen.
pub type TransferAskPass =
    Arc<dyn Fn(String, &mut AsyncApp) -> Task<Option<String>> + Send + Sync + 'static>;

/// A `remote_server` built before ADR-F28 decodes these messages into an empty
/// payload and answers nothing at all, and the channel to it has no request
/// timeout — so an old host would leave the transfer screen spinning forever.
/// The directory listing is the first thing the screen asks for, so it is the
/// one call given a deadline; once anything has come back, later commands
/// (which may legitimately run for minutes) are left unbounded.
const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

fn unsupported_message() -> &'static str {
    i18n::t!("The connected remote host does not support file transfer. Update zed-remote-server.")
}

#[derive(Clone, Default)]
pub(crate) struct RemoteExec {
    next_command_id: Arc<AtomicU64>,
    /// The askpass callback of every command still in flight, so a prompt the
    /// host relays reaches the caller that started that command.
    running: Arc<Mutex<HashMap<u64, TransferAskPass>>>,
    answered: Arc<AtomicBool>,
    unsupported: Arc<AtomicBool>,
}

impl RemoteExec {
    fn begin(&self, askpass: TransferAskPass) -> RunningCommand {
        let command_id = self.next_command_id.fetch_add(1, Ordering::SeqCst);
        self.running.lock().insert(command_id, askpass);
        RunningCommand {
            state: self.clone(),
            command_id,
        }
    }

    fn askpass(&self, command_id: u64) -> Option<TransferAskPass> {
        self.running.lock().get(&command_id).cloned()
    }

    fn running_command_ids(&self) -> Vec<u64> {
        self.running.lock().keys().copied().collect()
    }

    fn unsupported_error(&self) -> Option<anyhow::Error> {
        self.unsupported
            .load(Ordering::SeqCst)
            .then(|| anyhow!(unsupported_message()))
    }
}

/// Removes the command from [`RemoteExec::running`] however its task ends —
/// including when the caller drops the task, which is what the transfer screen
/// does when it is dismissed.
struct RunningCommand {
    state: RemoteExec,
    command_id: u64,
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        self.state.running.lock().remove(&self.command_id);
    }
}

impl Project {
    fn transfer_client(&self, cx: &App) -> Result<AnyProtoClient> {
        let remote = self
            .remote_client
            .as_ref()
            .context(i18n::t!("This project is not connected to a remote host."))?;
        Ok(remote.read(cx).proto_client())
    }

    /// Runs one transfer program on the remote host and waits for it to exit.
    ///
    /// `askpass` is called on the foreground whenever OpenSSH asks the process
    /// for a secret over there; returning `None` cancels the process. A
    /// non-zero exit is reported through [`TransferCommandOutput`] rather than
    /// as an error, so the caller can show the program's own stderr.
    pub fn run_transfer_command(
        &self,
        command: TransferCommand,
        askpass: TransferAskPass,
        cx: &mut Context<Self>,
    ) -> Task<Result<TransferCommandOutput>> {
        if let Some(error) = self.remote_exec.unsupported_error() {
            return Task::ready(Err(error));
        }
        let client = match self.transfer_client(cx) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        let state = self.remote_exec.clone();
        let running = state.begin(askpass);
        let command_id = running.command_id;
        let request = client.request(proto::RunTransferCommand {
            project_id: REMOTE_SERVER_PROJECT_ID,
            command_id,
            program: command.program.name().to_string(),
            args: command.args,
            stdin: command.stdin,
            cwd: command.working_directory,
            wants_askpass: command.wants_askpass,
        });
        cx.background_spawn(async move {
            let response = request.await;
            drop(running);
            let response = response?;
            state.answered.store(true, Ordering::SeqCst);
            Ok(TransferCommandOutput {
                stdout: response.stdout,
                stderr: response.stderr,
                exit_code: response.exit_code,
                cancelled: response.cancelled,
            })
        })
    }

    /// Stops every transfer command still running on the remote host. Used
    /// when the transfer screen disconnects or is dismissed, so a half-finished
    /// `scp` does not outlive the window that started it.
    pub fn cancel_transfer_commands(&self, cx: &mut Context<Self>) {
        let command_ids = self.remote_exec.running_command_ids();
        if command_ids.is_empty() || self.remote_exec.unsupported_error().is_some() {
            return;
        }
        let Some(client) = self.transfer_client(cx).log_err() else {
            return;
        };
        for command_id in command_ids {
            let request = client.request(proto::CancelTransferCommand {
                project_id: REMOTE_SERVER_PROJECT_ID,
                command_id,
            });
            cx.background_spawn(async move {
                request.await.log_err();
            })
            .detach();
        }
    }

    /// Lists a directory on the remote host, with the sizes and timestamps a
    /// file manager pane shows. Doubles as the capability probe described on
    /// [`CAPABILITY_PROBE_TIMEOUT`].
    pub fn list_transfer_directory(
        &self,
        path: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<RemoteDirectoryEntry>>> {
        if let Some(error) = self.remote_exec.unsupported_error() {
            return Task::ready(Err(error));
        }
        let client = match self.transfer_client(cx) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        let state = self.remote_exec.clone();
        let executor = cx.background_executor().clone();
        let request = client.request(proto::ListTransferDirectory {
            project_id: REMOTE_SERVER_PROJECT_ID,
            path,
        });
        cx.background_spawn(async move {
            let response = futures::select_biased! {
                response = request.fuse() => response,
                _ = executor.timer(CAPABILITY_PROBE_TIMEOUT).fuse() => {
                    // Nothing has ever come back, so the host is old rather
                    // than slow; remember that so the rest of the screen fails
                    // immediately instead of hanging too.
                    if !state.answered.load(Ordering::SeqCst) {
                        state.unsupported.store(true, Ordering::SeqCst);
                        return Err(anyhow!(unsupported_message()));
                    }
                    return Err(anyhow!(i18n::t!("The remote host is not responding.")));
                }
            }?;
            state.answered.store(true, Ordering::SeqCst);
            Ok(response
                .entries
                .into_iter()
                .map(|entry| RemoteDirectoryEntry {
                    kind: match entry.kind() {
                        proto::TransferEntryKind::TransferEntryDirectory => {
                            RemoteEntryKind::Directory
                        }
                        proto::TransferEntryKind::TransferEntrySymlink => RemoteEntryKind::Symlink,
                        proto::TransferEntryKind::TransferEntryFile => RemoteEntryKind::File,
                    },
                    name: entry.name,
                    size: entry.size,
                    mtime_epoch_seconds: entry.mtime_epoch_seconds,
                })
                .collect())
        })
    }

    /// Creates, renames or deletes on the remote host's own file system.
    pub fn transfer_file_operation(
        &self,
        operation: RemoteFileOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Some(error) = self.remote_exec.unsupported_error() {
            return Task::ready(Err(error));
        }
        let client = match self.transfer_client(cx) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        let state = self.remote_exec.clone();
        let operation = match operation {
            RemoteFileOperation::MakeDirectory { path } => {
                proto::transfer_file_operation::Operation::MakeDirectory(
                    proto::TransferMakeDirectory { path },
                )
            }
            RemoteFileOperation::Rename { from, to } => {
                proto::transfer_file_operation::Operation::Rename(proto::TransferRename {
                    from,
                    to,
                })
            }
            RemoteFileOperation::Remove { path, recursive } => {
                proto::transfer_file_operation::Operation::Remove(proto::TransferRemove {
                    path,
                    recursive,
                })
            }
        };
        let request = client.request(proto::TransferFileOperation {
            project_id: REMOTE_SERVER_PROJECT_ID,
            operation: Some(operation),
        });
        cx.background_spawn(async move {
            request.await?;
            state.answered.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    /// The remote host asked one of our transfer commands for a secret.
    pub(crate) async fn handle_transfer_askpass(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::TransferAskPassRequest>,
        mut cx: AsyncApp,
    ) -> Result<proto::TransferAskPassResponse> {
        let askpass = this.read_with(&cx, |this, _| {
            this.remote_exec.askpass(envelope.payload.command_id)
        });
        // The command has already been cancelled, or belongs to a screen that
        // has been closed; declining is what stops it over there.
        let Some(askpass) = askpass else {
            return Ok(proto::TransferAskPassResponse { response: None });
        };
        let response = askpass(envelope.payload.prompt, &mut cx).await;
        Ok(proto::TransferAskPassResponse { response })
    }
}
