//! ADR-F28 追記 (第 12 便): the 接続先 presets the transfer screen's picker
//! offers.
//!
//! There is deliberately **one** place a transfer target is saved: the quick
//! connect entries in `terminal_sessions.json`. A target saved from the
//! transfer screen goes back there with its transfer protocol attached
//! ([`SavedConnection::transfer_protocol`]), so the terminal's list and the
//! transfer screen's list can never drift apart, and a folder or a position
//! changed in one shows up in the other.
//!
//! Zed's own Remote SSH hosts (`remote.ssh_connections` in the settings file)
//! are offered beside them in a group of their own, read-only: they belong to
//! the settings file and are edited in the remote projects UI.

use gpui::SharedString;
use settings::SshConnection;
use terminal::terminal_macro::{ConnectionProtocol, SavedConnection};

use crate::{
    target_from_saved_connection,
    transfer_backend::{RemoteTarget, TransferProtocol},
};

/// Group heading for the hosts read out of `remote.ssh_connections`.
pub const REMOTE_SSH_GROUP: &str = i18n::msg!("Remote SSH");

/// Where a preset came from, and therefore what may be done with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetSource {
    /// A quick connect entry, identified by its position in
    /// `terminal_sessions.json`. That position is also the order it is shown
    /// in, and what 上へ / 下へ rewrite.
    QuickConnect { index: usize },
    /// A Remote SSH host from the settings file.
    RemoteSsh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionPreset {
    pub name: SharedString,
    /// The quick connect folder this entry is filed under, so selecting it
    /// fills the save form in. `None` for the Remote SSH hosts, which have no
    /// folder of their own.
    pub folder: Option<SharedString>,
    /// `プロトコル user@host:port`, for the row's second column.
    pub detail: SharedString,
    pub target: RemoteTarget,
    pub source: PresetSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetGroup {
    /// `None` for the entries that have no folder, which come first — the same
    /// order the quick connect modal lists them in.
    pub label: Option<SharedString>,
    pub presets: Vec<ConnectionPreset>,
}

/// The transfer target a saved connection stands for.
///
/// [`target_from_saved_connection`] derives one from the terminal protocol;
/// this adds what the entry itself says about file transfer, which is how an
/// SSH host can be marked "use SCP" or a Telnet device "FTPS on 2121".
pub fn preset_target(connection: &SavedConnection) -> RemoteTarget {
    let mut target = target_from_saved_connection(connection);
    if let Some(protocol) = connection
        .transfer_protocol
        .as_deref()
        .and_then(TransferProtocol::from_name)
    {
        // A protocol from the other family says nothing about the port it was
        // paired with — port 22 is meaningless for FTP — so the new protocol's
        // default takes over unless the entry names a port of its own.
        if protocol.is_ssh() != target.protocol.is_ssh() {
            target.port = protocol.default_port();
        }
        target.protocol = protocol;
    }
    if let Some(port) = connection.transfer_port {
        target.port = port;
    }
    target
}

/// The saved connection a 「接続先を保存」 writes, updating `existing` when the
/// name is already taken.
///
/// SFTP and SCP speak to the SSH daemon the terminal connects to, so both sides
/// of the entry describe one host and port. FTP has no terminal protocol at
/// all: an entry created for it defaults to Telnet (ADR-F28 pairs the two), and
/// an existing entry keeps whatever the terminal was using, with the FTP port
/// stored beside it.
pub fn preset_connection(
    existing: Option<&SavedConnection>,
    name: String,
    folder: Option<String>,
    target: &RemoteTarget,
    remember_password: bool,
) -> SavedConnection {
    let mut connection = existing.cloned().unwrap_or_default();
    connection.name = name;
    connection.folder = folder;
    connection.host = target.host.clone();
    connection.user = target.user.clone();
    connection.remember_password = remember_password;
    connection.transfer_protocol = Some(target.protocol.name().to_string());
    if target.protocol.is_ssh() {
        connection.protocol = ConnectionProtocol::Ssh;
        connection.port = Some(target.port);
        connection.transfer_port = None;
    } else {
        if existing.is_none() {
            connection.protocol = ConnectionProtocol::Telnet;
            connection.port = None;
        }
        connection.transfer_port = Some(target.port);
    }
    connection
}

/// Builds the picker's contents: the quick connect entries grouped by folder,
/// then the Remote SSH hosts.
///
/// Folders are ordered by name (the unfoldered entries first, as an empty
/// name), and inside a folder the entries keep the order the file has them in —
/// which is what [`move_connection`] rewrites.
pub fn preset_groups(
    connections: &[SavedConnection],
    ssh_connections: &[SshConnection],
) -> Vec<PresetGroup> {
    let mut groups: Vec<PresetGroup> = Vec::new();
    for (index, connection) in connections.iter().enumerate() {
        let target = preset_target(connection);
        let label = connection.folder.clone().map(SharedString::from);
        let preset = ConnectionPreset {
            name: SharedString::from(connection.name.clone()),
            folder: label.clone(),
            detail: SharedString::from(target.describe()),
            target,
            source: PresetSource::QuickConnect { index },
        };
        match groups.iter_mut().find(|group| group.label == label) {
            Some(group) => group.presets.push(preset),
            None => groups.push(PresetGroup {
                label,
                presets: vec![preset],
            }),
        }
    }
    groups.sort_by(|left, right| {
        left.label
            .as_deref()
            .unwrap_or_default()
            .cmp(right.label.as_deref().unwrap_or_default())
    });

    let remote_ssh = ssh_connections
        .iter()
        .filter_map(remote_ssh_preset)
        .collect::<Vec<_>>();
    if !remote_ssh.is_empty() {
        groups.push(PresetGroup {
            label: Some(SharedString::from(i18n::tr(REMOTE_SSH_GROUP))),
            presets: remote_ssh,
        });
    }
    groups
}

/// A Remote SSH host as a transfer target. SFTP is the only sensible reading of
/// a Zed remote development entry: it is an SSH host by definition.
fn remote_ssh_preset(connection: &SshConnection) -> Option<ConnectionPreset> {
    let host = connection.host.trim();
    if host.is_empty() {
        return None;
    }
    let user = connection
        .username
        .as_deref()
        .map(str::trim)
        .filter(|user| !user.is_empty())
        .map(str::to_string);
    let target = RemoteTarget {
        protocol: TransferProtocol::Sftp,
        host: host.to_string(),
        port: connection
            .port
            .unwrap_or_else(|| TransferProtocol::Sftp.default_port()),
        user,
    };
    Some(ConnectionPreset {
        folder: None,
        name: SharedString::from(
            connection
                .nickname
                .clone()
                .filter(|nickname| !nickname.trim().is_empty())
                .unwrap_or_else(|| host.to_string()),
        ),
        detail: SharedString::from(target.describe()),
        target,
        source: PresetSource::RemoteSsh,
    })
}

/// The entry the one at `index` swaps with when it is moved. Entries of other
/// folders in between are skipped, because the picker does not show them
/// between these two.
fn neighbour_in_folder(connections: &[SavedConnection], index: usize, up: bool) -> Option<usize> {
    let folder = connections.get(index)?.folder.as_deref();
    let same_folder = |candidate: &usize| {
        connections
            .get(*candidate)
            .is_some_and(|connection| connection.folder.as_deref() == folder)
    };
    if up {
        (0..index).rev().find(same_folder)
    } else {
        ((index + 1)..connections.len()).find(same_folder)
    }
}

/// Whether 上へ / 下へ would do anything, so the button can say so.
pub fn can_move_connection(connections: &[SavedConnection], index: usize, up: bool) -> bool {
    neighbour_in_folder(connections, index, up).is_some()
}

/// Moves the connection at `index` one row up or down *inside its own folder*,
/// which is the order the picker shows. `None` when it is already at the edge
/// of its group, so the caller can leave the file alone.
pub fn move_connection(
    connections: &[SavedConnection],
    index: usize,
    up: bool,
) -> Option<Vec<SavedConnection>> {
    let neighbour = neighbour_in_folder(connections, index, up)?;
    let mut reordered = connections.to_vec();
    reordered.swap(index, neighbour);
    Some(reordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(name: &str, folder: Option<&str>) -> SavedConnection {
        SavedConnection {
            name: name.to_string(),
            folder: folder.map(str::to_string),
            host: format!("{name}.example.com"),
            ..SavedConnection::default()
        }
    }

    fn ssh_connection(host: &str, nickname: Option<&str>) -> SshConnection {
        SshConnection {
            host: host.to_string(),
            username: Some("ubuntu".to_string()),
            port: Some(2200),
            nickname: nickname.map(str::to_string),
            ..SshConnection::default()
        }
    }

    #[test]
    fn groups_follow_the_folder_then_the_saved_order() {
        let connections = vec![
            connection("z", Some("拠点/本社")),
            connection("a", None),
            connection("m", Some("拠点/本社")),
            connection("b", None),
            connection("q", Some("拠点/支社")),
        ];
        let groups = preset_groups(&connections, &[]);
        let shape = groups
            .iter()
            .map(|group| {
                (
                    group.label.as_deref().unwrap_or_default().to_string(),
                    group
                        .presets
                        .iter()
                        .map(|preset| preset.name.to_string())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                // The unfoldered entries first, then the folders by name (支 is
                // before 本); the entries inside a folder keep the file's order
                // rather than being sorted by name.
                ("".to_string(), vec!["a".to_string(), "b".to_string()]),
                ("拠点/支社".to_string(), vec!["q".to_string()]),
                (
                    "拠点/本社".to_string(),
                    vec!["z".to_string(), "m".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn remote_ssh_hosts_come_last_in_their_own_group() {
        let groups = preset_groups(
            &[connection("a", None)],
            &[
                ssh_connection("10.0.0.9", Some("検証機")),
                ssh_connection("", None),
            ],
        );
        let last = groups.last().expect("a group for the remote hosts");
        assert_eq!(last.label.as_deref(), Some(REMOTE_SSH_GROUP));
        // The entry without a host is not a target and is dropped.
        assert_eq!(last.presets.len(), 1);
        let preset = &last.presets[0];
        assert_eq!(preset.name.as_ref(), "検証機");
        assert_eq!(preset.source, PresetSource::RemoteSsh);
        assert_eq!(preset.target.protocol, TransferProtocol::Sftp);
        assert_eq!(preset.target.port, 2200);
        assert_eq!(preset.target.user.as_deref(), Some("ubuntu"));
    }

    #[test]
    fn the_entry_transfer_protocol_wins_over_the_terminal_one() {
        let mut ssh = connection("r1", None);
        ssh.port = Some(2222);
        assert_eq!(preset_target(&ssh).protocol, TransferProtocol::Sftp);
        assert_eq!(preset_target(&ssh).port, 2222);

        ssh.transfer_protocol = Some("scp".to_string());
        assert_eq!(preset_target(&ssh).protocol, TransferProtocol::Scp);
        // Same family, so the SSH port carries over.
        assert_eq!(preset_target(&ssh).port, 2222);

        ssh.transfer_protocol = Some("ftps".to_string());
        let target = preset_target(&ssh);
        assert_eq!(target.protocol, TransferProtocol::Ftps);
        // Port 2222 belongs to the SSH daemon and means nothing to FTPS.
        assert_eq!(target.port, 990);

        ssh.transfer_port = Some(2121);
        assert_eq!(preset_target(&ssh).port, 2121);

        // An unreadable value falls back rather than refusing to connect.
        ssh.transfer_protocol = Some("rsync".to_string());
        ssh.transfer_port = None;
        assert_eq!(preset_target(&ssh).protocol, TransferProtocol::Sftp);
    }

    #[test]
    fn saving_a_preset_round_trips_through_the_saved_connection() {
        for protocol in TransferProtocol::ALL {
            let target = RemoteTarget {
                protocol,
                host: "10.0.0.1".to_string(),
                port: 2121,
                user: Some("operator".to_string()),
            };
            let saved = preset_connection(
                None,
                "本社".to_string(),
                Some("拠点".to_string()),
                &target,
                true,
            );
            assert_eq!(preset_target(&saved), target, "{protocol:?}");
            assert!(saved.remember_password);
            assert_eq!(saved.folder.as_deref(), Some("拠点"));
        }
    }

    #[test]
    fn an_ftp_preset_leaves_an_existing_terminal_entry_alone() {
        let mut existing = connection("機器", None);
        existing.protocol = ConnectionProtocol::Telnet;
        existing.port = Some(2323);
        existing.macro_name = Some("ログイン".to_string());
        let target = RemoteTarget {
            protocol: TransferProtocol::Ftp,
            host: "10.0.0.2".to_string(),
            port: 2121,
            user: None,
        };
        let saved = preset_connection(Some(&existing), existing.name.clone(), None, &target, false);
        // The terminal still telnets to 2323, and its macro survives.
        assert_eq!(saved.protocol, ConnectionProtocol::Telnet);
        assert_eq!(saved.port, Some(2323));
        assert_eq!(saved.macro_name.as_deref(), Some("ログイン"));
        assert_eq!(preset_target(&saved), target);
    }

    #[test]
    fn moving_an_entry_swaps_it_with_its_neighbour_in_the_same_folder() {
        let connections = vec![
            connection("z", Some("拠点")),
            connection("a", None),
            connection("m", Some("拠点")),
        ];
        let names = |connections: &[SavedConnection]| {
            connections
                .iter()
                .map(|connection| connection.name.clone())
                .collect::<Vec<_>>()
        };

        // "m" moves past the unfoldered entry between it and "z".
        let moved = move_connection(&connections, 2, true).expect("m can move up");
        assert_eq!(names(&moved), vec!["m", "a", "z"]);
        // The edges of a group have nowhere to go.
        assert!(move_connection(&connections, 0, true).is_none());
        assert!(move_connection(&connections, 2, false).is_none());
        assert!(move_connection(&connections, 1, false).is_none());
        assert!(move_connection(&connections, 9, true).is_none());
    }
}
