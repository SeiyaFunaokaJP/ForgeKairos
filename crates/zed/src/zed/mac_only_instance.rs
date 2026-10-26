// Compiled into the tests on every platform so the port and handshake rules
// can be checked off macOS; only the macOS app calls the rest.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    sync::LazyLock,
    thread,
    time::Duration,
};

use release_channel::ReleaseChannel;

const LOCALHOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(10);
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(35);
const SEND_TIMEOUT: Duration = Duration::from_millis(20);

fn address() -> SocketAddr {
    // These port numbers are offset by the user ID to avoid conflicts between
    // different users on the same machine. In addition to that the ports for each
    // release channel are spaced out by 100 to avoid conflicts between different
    // users running different release channels on the same machine. This ends up
    // interleaving the ports between different users and different release channels.
    //
    // On macOS user IDs start at 501 and on Linux they start at 1000. The first user
    // on a Mac with ID 501 running a dev channel build will use port 34238, and the
    // second user with ID 502 will use port 34239, and so on. User 501 will use ports
    // 34338, 34438, and 34538 for the preview, stable, and nightly channels,
    // respectively. User 502 will use ports 34339, 34439, and 34539 for the preview,
    // stable, and nightly channels, respectively.
    let user_id = current_user_id();

    SocketAddr::V4(SocketAddrV4::new(
        LOCALHOST,
        instance_port(*release_channel::RELEASE_CHANNEL, user_id),
    ))
}

// Read directly rather than by enumerating processes, which slowed startup.
#[cfg(target_os = "macos")]
fn current_user_id() -> Option<u32> {
    Some(nix::unistd::getuid().as_raw())
}

#[cfg(not(target_os = "macos"))]
fn current_user_id() -> Option<u32> {
    None
}

// zed-jp-enhance: an official Zed (and this fork before it was renamed) owns
// every port from `LEGACY_MAC_INSTANCE_BASE_PORT` up, so this fork's ports sit
// below it and never answer, or get answered by, an official instance. The
// `paths` crate probes the same ports, so both compute them in `branding`.
fn instance_port(channel: ReleaseChannel, user_id: Option<u32>) -> u16 {
    let ports = branding::MAC_INSTANCE_PORTS.ports(user_id);
    match channel {
        ReleaseChannel::Dev => ports.dev,
        ReleaseChannel::Nightly => ports.nightly,
        ReleaseChannel::Preview => ports.preview,
        ReleaseChannel::Stable => ports.stable,
    }
}

fn instance_handshake() -> &'static str {
    static HANDSHAKE: LazyLock<String> =
        LazyLock::new(|| handshake_for(release_channel::app_identifier()));
    HANDSHAKE.as_str()
}

fn handshake_for(app_identifier: &str) -> String {
    branding::mac_instance_handshake(app_identifier)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsOnlyInstance {
    Yes,
    No,
}

pub fn ensure_only_instance() -> IsOnlyInstance {
    if check_got_handshake() {
        return IsOnlyInstance::No;
    }

    let listener = match TcpListener::bind(address()) {
        Ok(listener) => listener,

        Err(err) => {
            log::warn!("Error binding to single instance port: {err}");
            if check_got_handshake() {
                return IsOnlyInstance::No;
            }

            // Avoid failing to start when some other application by chance already has
            // a claim on the port. This is sub-par as any other instance that gets launched
            // will be unable to communicate with this instance and will duplicate
            log::warn!("Backup handshake request failed, continuing without handshake");
            return IsOnlyInstance::Yes;
        }
    };

    thread::Builder::new()
        .name("EnsureSingleton".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => return,
                };

                _ = stream.set_nodelay(true);
                _ = stream.set_read_timeout(Some(SEND_TIMEOUT));
                _ = stream.write_all(instance_handshake().as_bytes());
            }
        })
        .unwrap();

    IsOnlyInstance::Yes
}

fn check_got_handshake() -> bool {
    check_got_handshake_at(&address(), instance_handshake())
}

fn check_got_handshake_at(address: &SocketAddr, handshake: &str) -> bool {
    match TcpStream::connect_timeout(address, CONNECT_TIMEOUT) {
        Ok(mut stream) => {
            let mut buf = vec![0u8; handshake.len()];

            if let Err(err) = stream.set_read_timeout(Some(RECEIVE_TIMEOUT)) {
                log::warn!("Failed to set a timeout on the single instance port: {err}");
                return false;
            }
            if let Err(err) = stream.read_exact(&mut buf) {
                log::warn!("Connected to single instance port but failed to read: {err}");
                return false;
            }

            if buf == handshake.as_bytes() {
                log::info!("Got instance handshake");
                return true;
            }

            log::warn!("Got wrong instance handshake value");
            false
        }

        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNELS: [ReleaseChannel; 4] = [
        ReleaseChannel::Dev,
        ReleaseChannel::Nightly,
        ReleaseChannel::Preview,
        ReleaseChannel::Stable,
    ];

    const OFFICIAL_HANDSHAKES: [&str; 4] = [
        "Zed Editor Dev Instance Running",
        "Zed Editor Nightly Instance Running",
        "Zed Editor Preview Instance Running",
        "Zed Editor Stable Instance Running",
    ];

    const OFFICIAL_BASE_PORT: u16 = 43737;

    fn own_handshake(channel: ReleaseChannel) -> String {
        handshake_for(channel.branded(&branding::CURRENT.app_identifiers))
    }

    /// An official Zed's port, as upstream's `address` computes it.
    fn official_port(channel: ReleaseChannel, user_id: u32) -> u16 {
        let port = match channel {
            ReleaseChannel::Dev => OFFICIAL_BASE_PORT,
            ReleaseChannel::Preview => OFFICIAL_BASE_PORT + 100,
            ReleaseChannel::Stable => OFFICIAL_BASE_PORT + 200,
            ReleaseChannel::Nightly => OFFICIAL_BASE_PORT + 300,
        };
        let max_uid = 65535 - u32::from(port);
        port + (user_id % max_uid) as u16
    }

    fn user_ids() -> impl Iterator<Item = u32> {
        (0..70_000)
            .chain((u32::MAX - 70_000)..=u32::MAX)
            .chain((0..32).map(|shift| 1u32 << shift))
            .chain([1_000_000_000, 2_000_000_001])
    }

    #[test]
    fn handshakes_differ_from_the_official_ones() {
        for channel in CHANNELS {
            let handshake = own_handshake(channel);
            for official in OFFICIAL_HANDSHAKES {
                // The reader compares a prefix of its own handshake's length,
                // so neither may be a prefix of the other.
                assert!(
                    !official.starts_with(handshake.as_str()) && !handshake.starts_with(official),
                    "{handshake:?} would be taken for the official {official:?}"
                );
            }
        }
    }

    #[test]
    fn the_legacy_constants_are_the_official_values() {
        assert_eq!(
            branding::LEGACY_MAC_INSTANCE_HANDSHAKES.all(),
            OFFICIAL_HANDSHAKES
        );
        assert_eq!(branding::LEGACY_MAC_INSTANCE_BASE_PORT, OFFICIAL_BASE_PORT);
    }

    #[test]
    fn ports_never_fall_in_the_official_range() {
        let official_lowest = CHANNELS
            .into_iter()
            .flat_map(|channel| user_ids().map(move |user_id| official_port(channel, user_id)))
            .min();
        assert_eq!(official_lowest, Some(OFFICIAL_BASE_PORT));

        for channel in CHANNELS {
            for user_id in user_ids().map(Some).chain([None]) {
                let port = instance_port(channel, user_id);
                assert!(
                    (branding::MAC_INSTANCE_PORTS.base_port..OFFICIAL_BASE_PORT).contains(&port),
                    "{channel:?} user {user_id:?}: port {port} is in the official range"
                );
            }
        }
    }

    #[test]
    fn channels_of_one_user_get_different_ports() {
        for user_id in [0, 501, 502, 1000, 65_534] {
            let mut ports: Vec<u16> = CHANNELS
                .into_iter()
                .map(|channel| instance_port(channel, Some(user_id)))
                .collect();
            ports.sort_unstable();
            ports.dedup();
            assert_eq!(ports.len(), 4, "user {user_id}");
        }
    }

    fn serve_once(handshake: String) -> SocketAddr {
        let listener = TcpListener::bind(SocketAddrV4::new(LOCALHOST, 0)).expect("bind");
        let address = listener.local_addr().expect("local address");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.write_all(handshake.as_bytes()).ok();
            }
        });
        address
    }

    #[test]
    fn an_official_instance_does_not_count_as_this_app_running() {
        for (channel, official) in CHANNELS.into_iter().zip(OFFICIAL_HANDSHAKES) {
            let handshake = own_handshake(channel);
            let official_instance = serve_once(official.to_string());
            assert!(
                !check_got_handshake_at(&official_instance, &handshake),
                "{channel:?}"
            );

            let own_instance = serve_once(handshake.clone());
            assert!(
                check_got_handshake_at(&own_instance, &handshake),
                "{channel:?}"
            );
        }
    }
}
