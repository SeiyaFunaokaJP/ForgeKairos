//! ADR-F28: WinSCP-style file transfer (SFTP / SCP / FTP / FTPS) driven from
//! the terminal.
//!
//! This crate owns the `terminal::OpenFileTransfer` handler, the two-pane
//! modal and the OS-command backends. `terminal_view` only ever *dispatches*
//! the action (from the terminal's context menu, the quick connect modal and
//! the terminal tab bar), which is why the action itself lives in
//! `zed_actions` — that keeps this crate free to depend on `terminal_view`
//! for reading the active terminal.
//!
//! Transfers are started by the user pressing a button, so ADR-F18 / F19
//! treat them like typing into a terminal: outside the egress allow list, and
//! deliberately not exposed as an agent tool.

mod file_transfer_modal;
mod transfer_backend;

use gpui::{App, Window};
use terminal::terminal_macro::{ConnectionProtocol, SavedConnection, TerminalSessionStore};
use workspace::Workspace;
use zed_actions::terminal::OpenFileTransfer;

pub use file_transfer_modal::{
    FileTransferModal, LocalSide, PaneSide, RemoteHostOptions, TransferHostOptions,
    open_file_transfer,
};
pub use transfer_backend::{ExecHost, IdeExecHost, RemoteExecHost, RemoteTarget, TransferProtocol};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, action: &OpenFileTransfer, window, cx| {
            let target = target_from_action(action)
                .or_else(|| active_terminal_target(workspace, window, cx));
            let store = TerminalSessionStore::global(workspace.app_state().fs.clone(), cx);
            open_file_transfer(workspace, store, target, window, cx);
        });
    })
    .detach();
}

/// The quick-connect entry a terminal was started from, mapped onto a
/// transfer protocol.
///
/// SSH carries its port over, since `sftp` and `scp` speak to the same
/// daemon. Telnet does not: it has no transfer protocol of its own, so FTP is
/// offered instead, and Telnet's port 23 has nothing to do with FTP's 21.
pub fn target_from_saved_connection(connection: &SavedConnection) -> RemoteTarget {
    let (protocol, port) = match connection.protocol {
        ConnectionProtocol::Ssh => (TransferProtocol::Sftp, connection.port_or_default()),
        ConnectionProtocol::Telnet => (TransferProtocol::Ftp, TransferProtocol::Ftp.default_port()),
    };
    RemoteTarget {
        protocol,
        host: connection.host.clone(),
        port,
        user: connection.user.clone(),
    }
}

/// The host named by the action itself, as the quick connect modal's
/// 「ファイル転送を開く」 button and a keymap entry can do. Without a host
/// there is nothing to pre-fill and the active terminal is consulted instead.
fn target_from_action(action: &OpenFileTransfer) -> Option<RemoteTarget> {
    let host = action
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())?;
    let protocol = action
        .protocol
        .as_deref()
        .and_then(TransferProtocol::from_name)
        .unwrap_or_default();
    Some(RemoteTarget {
        protocol,
        host: host.to_string(),
        port: action.port.unwrap_or_else(|| protocol.default_port()),
        user: action
            .user
            .as_deref()
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .map(str::to_string),
    })
}

/// The directory the active terminal is sitting in *on the remote host*, so a
/// transfer screen opened from it starts where the user is working rather than
/// at the project root (ADR-F28). `None` for a local terminal, or before the
/// remote shell has reported a directory.
pub(crate) fn active_remote_working_directory(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<std::path::PathBuf> {
    let view = terminal_view::session_tools::active_terminal_view(workspace, window, cx)?;
    view.read(cx).terminal().read(cx).remote_working_directory()
}

fn active_terminal_target(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<RemoteTarget> {
    let view = terminal_view::session_tools::active_terminal_view(workspace, window, cx)?;
    let connection = view
        .read(cx)
        .terminal()
        .read(cx)
        .connection_origin()?
        .clone();
    Some(target_from_saved_connection(&connection))
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    use super::*;

    fn saved_ssh_connection() -> SavedConnection {
        SavedConnection {
            name: "本社ルーター".to_string(),
            protocol: ConnectionProtocol::Ssh,
            host: "10.0.0.1".to_string(),
            port: Some(2222),
            user: Some("operator".to_string()),
            ..SavedConnection::default()
        }
    }

    #[test]
    fn maps_ssh_to_sftp_and_telnet_to_ftp() {
        let ssh = target_from_saved_connection(&saved_ssh_connection());
        assert_eq!(ssh.protocol, TransferProtocol::Sftp);
        assert_eq!(ssh.host, "10.0.0.1");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.user.as_deref(), Some("operator"));

        let telnet = target_from_saved_connection(&SavedConnection {
            protocol: ConnectionProtocol::Telnet,
            port: Some(2300),
            ..saved_ssh_connection()
        });
        assert_eq!(telnet.protocol, TransferProtocol::Ftp);
        // Telnet's port is meaningless for FTP, so the FTP default is used.
        assert_eq!(telnet.port, 21);
    }

    #[test]
    fn reads_the_target_out_of_the_action() {
        assert!(target_from_action(&OpenFileTransfer::default()).is_none());
        let target = target_from_action(&OpenFileTransfer {
            host: Some(" files.example.com ".to_string()),
            port: None,
            user: Some("ops".to_string()),
            protocol: Some("ftps".to_string()),
        })
        .expect("a host makes a target");
        assert_eq!(target.protocol, TransferProtocol::Ftps);
        assert_eq!(target.host, "files.example.com");
        assert_eq!(target.port, 990);
        assert_eq!(target.user.as_deref(), Some("ops"));
    }

    #[gpui::test]
    async fn the_action_opens_the_modal_with_the_target_filled_in(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), serde_json::json!({ "readme.md": "" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace: Entity<workspace::Workspace> = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the test window should have a workspace");
        let cx = &mut VisualTestContext::from_window(*window, cx);
        cx.run_until_parked();

        cx.dispatch_action(OpenFileTransfer {
            host: Some("10.0.0.1".to_string()),
            port: Some(2222),
            user: Some("operator".to_string()),
            protocol: Some("sftp".to_string()),
        });
        cx.run_until_parked();

        let modal: Entity<FileTransferModal> = workspace
            .update(cx, |workspace, cx| {
                workspace.active_modal::<FileTransferModal>(cx)
            })
            .expect("the transfer screen should be the active modal");

        let (protocol, host, port, user) = modal.update(cx, |modal, cx| modal.form_state(cx));
        assert_eq!(protocol, TransferProtocol::Sftp);
        assert_eq!(host, "10.0.0.1");
        assert_eq!(port, "2222");
        assert_eq!(user, "operator");

        // The local pane starts at the project's first worktree.
        let local = modal.update(cx, |modal, cx| modal.local_directory_for_test(cx));
        assert_eq!(local, path!("/project"));
    }
}
