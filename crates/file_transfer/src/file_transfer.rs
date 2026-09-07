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

mod connection_presets;
mod file_transfer_modal;
mod transfer_backend;

use gpui::{App, Window};
use terminal::terminal_macro::{ConnectionProtocol, SavedConnection, TerminalSessionStore};
use workspace::Workspace;
use zed_actions::terminal::OpenFileTransfer;

pub use connection_presets::{ConnectionPreset, PresetGroup, PresetSource, preset_groups};
pub use file_transfer_modal::{
    FileTransferModal, FocusNextField, FocusPreviousField, LocalSide, PaneSide, RemoteHostOptions,
    TransferHostOptions, open_file_transfer,
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
    use std::path::PathBuf;

    use fs::Fs as _;
    use gpui::{AppContext as _, Entity, KeyBinding, TestAppContext, VisualTestContext};
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

    #[gpui::test]
    async fn the_preset_picker_groups_by_folder_and_writes_back_to_the_shared_file(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/config"),
            serde_json::json!({
                "terminal_sessions.json": r#"{
                    "connections": [
                        { "name": "本社ルーター", "folder": "拠点/本社",
                          "host": "10.0.0.1", "port": 2222, "user": "operator" },
                        { "name": "検証機", "host": "10.0.0.2" },
                        { "name": "支社スイッチ", "folder": "拠点/本社", "protocol": "telnet",
                          "host": "10.0.0.3", "transfer_protocol": "ftps", "transfer_port": 2121 }
                    ]
                }"#
            }),
        )
        .await;
        let sessions_path = PathBuf::from(path!("/config/terminal_sessions.json"));
        let store = cx.update(|cx| {
            cx.new(|cx| TerminalSessionStore::new(fs.clone(), sessions_path.clone(), cx))
        });
        let window = cx.add_window(|window, cx| {
            FileTransferModal::new(
                store.clone(),
                TransferHostOptions {
                    fs: fs.clone(),
                    ide_root: PathBuf::from(path!("/project")),
                    remote: None,
                },
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Unfoldered entries first, then the folder, and inside it the order
        // the file has rather than the alphabet.
        let rows = window
            .update(cx, |modal, _, cx| modal.preset_rows_for_test(cx))
            .expect("the window is open");
        assert_eq!(
            rows,
            vec![
                "/検証機".to_string(),
                "拠点/本社/本社ルーター".to_string(),
                "拠点/本社/支社スイッチ".to_string(),
            ]
        );

        // The entry's own transfer protocol and port win over the terminal's.
        let form = window
            .update(cx, |modal, window, cx| {
                assert!(modal.apply_preset_for_test("支社スイッチ", window, cx));
                modal.form_state(cx)
            })
            .expect("the window is open");
        assert_eq!(
            form,
            (
                TransferProtocol::Ftps,
                "10.0.0.3".to_string(),
                "2121".to_string(),
                String::new()
            )
        );

        window
            .update(cx, |modal, _, cx| {
                assert!(modal.move_preset_for_test("支社スイッチ", true, cx));
            })
            .expect("the window is open");
        cx.run_until_parked();
        let rows = window
            .update(cx, |modal, _, cx| modal.preset_rows_for_test(cx))
            .expect("the window is open");
        assert_eq!(
            rows,
            vec![
                "/検証機".to_string(),
                "拠点/本社/支社スイッチ".to_string(),
                "拠点/本社/本社ルーター".to_string(),
            ]
        );

        window
            .update(cx, |modal, window, cx| {
                modal.save_preset_for_test("新しい機器", "拠点/支社", window, cx);
            })
            .expect("the window is open");
        cx.run_until_parked();

        let saved = store.read_with(cx, |store, _| {
            store
                .connections()
                .iter()
                .find(|connection| connection.name == "新しい機器")
                .cloned()
        });
        let saved = saved.expect("the preset should be written to the shared file");
        assert_eq!(saved.folder.as_deref(), Some("拠点/支社"));
        assert_eq!(saved.host, "10.0.0.3");
        // FTPS has no terminal protocol of its own, so a new entry is a Telnet
        // one carrying the transfer protocol and port beside it.
        assert_eq!(saved.protocol, ConnectionProtocol::Telnet);
        assert_eq!(saved.port, None);
        assert_eq!(saved.transfer_protocol.as_deref(), Some("ftps"));
        assert_eq!(saved.transfer_port, Some(2121));

        let written = fs
            .load(&sessions_path)
            .await
            .expect("the file should have been written");
        assert!(written.contains("新しい機器"), "{written}");
    }

    /// Tab has to walk the transfer screen's own fields and wrap back into it.
    ///
    /// The modal is opened inside a real workspace on purpose: the status bar's
    /// dock buttons are `tab_index(0isize)` tab stops that paint *after* the
    /// modal layer, so in a window holding nothing but the modal the bug this
    /// covers cannot happen at all. The keystroke is dispatched rather than the
    /// action, so the keymap context that carries it is exercised too.
    #[gpui::test]
    async fn tab_cycles_the_transfer_fields_and_wraps_inside_the_modal(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
            // The same two contexts assets/keymaps/default-windows.json binds.
            cx.bind_keys([
                KeyBinding::new("tab", FocusNextField, Some("FileTransfer")),
                KeyBinding::new("shift-tab", FocusPreviousField, Some("FileTransfer")),
                KeyBinding::new("tab", FocusNextField, Some("FileTransfer > Editor")),
                KeyBinding::new(
                    "shift-tab",
                    FocusPreviousField,
                    Some("FileTransfer > Editor"),
                ),
            ]);
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
            ..OpenFileTransfer::default()
        });
        cx.run_until_parked();

        let modal: Entity<FileTransferModal> = workspace
            .update(cx, |workspace, cx| {
                workspace.active_modal::<FileTransferModal>(cx)
            })
            .expect("the transfer screen should be the active modal");

        let focused_field = |modal: &Entity<FileTransferModal>, cx: &mut VisualTestContext| {
            modal.update_in(cx, |modal, window, cx| {
                modal.focused_field_for_test(window, cx)
            })
        };

        // `FileTransferModal::new` focuses the host field itself, but this makes
        // the starting point explicit rather than relying on it.
        modal.update_in(cx, |modal, window, cx| {
            modal.focus_host_for_test(window, cx)
        });
        cx.run_until_parked();
        assert_eq!(focused_field(&modal, cx), Some("host"));

        // The 接続先 pane is closed, so its two fields are not in the order;
        // the last Tab wraps back to ホスト instead of leaving the modal.
        for expected in [
            "port",
            "user",
            "password",
            "local_path",
            "remote_path",
            "host",
        ] {
            cx.simulate_keystrokes("tab");
            assert_eq!(
                focused_field(&modal, cx),
                Some(expected),
                "tab should reach {expected}"
            );
        }

        for expected in [
            "remote_path",
            "local_path",
            "password",
            "user",
            "port",
            "host",
        ] {
            cx.simulate_keystrokes("shift-tab");
            assert_eq!(
                focused_field(&modal, cx),
                Some(expected),
                "shift-tab should reach {expected}"
            );
        }
    }
}
