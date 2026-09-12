use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use prost::Message as _;
use rpc::proto::Envelope;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct MessageId(pub u32);

pub type MessageLen = u32;
pub const MESSAGE_LEN_SIZE: usize = size_of::<MessageLen>();

pub fn message_len_from_buffer(buffer: &[u8]) -> MessageLen {
    MessageLen::from_le_bytes(buffer.try_into().unwrap())
}

pub async fn read_message_with_len<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message_len: MessageLen,
) -> Result<Envelope> {
    buffer.resize(message_len as usize, 0);
    stream.read_exact(buffer).await?;
    Ok(Envelope::decode(buffer.as_slice())?)
}

pub async fn read_message<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<Envelope> {
    buffer.resize(MESSAGE_LEN_SIZE, 0);
    stream.read_exact(buffer).await?;

    let len = message_len_from_buffer(buffer);

    read_message_with_len(stream, buffer, len).await
}

pub async fn write_message<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message: Envelope,
) -> Result<()> {
    let message_len = message.encoded_len() as u32;
    stream
        .write_all(message_len.to_le_bytes().as_slice())
        .await?;
    buffer.clear();
    buffer.reserve(message_len as usize);
    message.encode(buffer)?;
    stream.write_all(buffer).await?;
    Ok(())
}

pub async fn write_size_prefixed_buffer<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let len = buffer.len() as u32;
    stream.write_all(len.to_le_bytes().as_slice()).await?;
    stream.write_all(buffer).await?;
    Ok(())
}

pub async fn read_message_raw<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    buffer.resize(MESSAGE_LEN_SIZE, 0);
    stream.read_exact(buffer).await?;

    let message_len = message_len_from_buffer(buffer);
    buffer.resize(message_len as usize, 0);
    stream.read_exact(buffer).await?;

    Ok(())
}

// -------------------------------------------------------------------------
// ADR-F45: the socket between a `remote_server proxy` and the one daemon that
// serves every session of its client on this host.
// -------------------------------------------------------------------------

/// Guards the framing below. The proxy and the daemon are always the same
/// binary, so this only matters for a daemon that outlived an upgrade and
/// happens to share the new build's key.
pub const DAEMON_PROTOCOL_VERSION: u32 = 1;

/// The introduction is a few dozen bytes; anything much larger is not one.
const MAX_DAEMON_JSON_MESSAGE_LEN: MessageLen = 64 * 1024;

/// Separates the client instance id from the session name in a connection
/// identifier (`<client id>.<session>`), so that the proxy can pick the daemon
/// that belongs to the client without the transports having to carry a second
/// argument.
///
/// It has to be a character a file name can hold: a remote server from before
/// ADR-F45 names its state directory and its log file after the identifier.
pub const CLIENT_ID_SEPARATOR: char = '.';

/// The largest frame either end accepts. Far beyond any envelope Zed builds —
/// the biggest is a buffer's full text — and far short of an allocation that
/// could take the host down over one corrupt length.
pub const MAX_DAEMON_FRAME_LEN: MessageLen = 256 * 1024 * 1024;

/// The first thing a proxy sends on a fresh connection to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHello {
    pub protocol_version: u32,
    /// The session this connection belongs to: one per remote project window,
    /// or per connection dialog (`setup-N`).
    pub identifier: String,
    /// Whether the client is resuming a session it already has, rather than
    /// starting it afresh.
    pub reconnect: bool,
    /// The proxy's `SSH_AUTH_SOCK`. Each ssh session forwards the user's agent
    /// through a socket of its own, and the one the daemon started with goes
    /// away with the session that started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_auth_sock: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonHelloOutcome {
    /// The connection now carries the session; frames follow.
    Accepted,
    /// A reconnect named a session the daemon no longer has (it idled out, or
    /// the daemon was restarted), so there is nothing to resume.
    SessionNotFound,
    /// The daemon cannot serve this connection; the reply says why.
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHelloReply {
    pub outcome: DaemonHelloOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Writes a length-prefixed JSON message (the introduction and its reply).
pub async fn write_json_message<S: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut S,
    message: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(message).context("encoding a daemon message")?;
    let len = MessageLen::try_from(bytes.len())
        .ok()
        .filter(|len| *len <= MAX_DAEMON_JSON_MESSAGE_LEN)
        .with_context(|| format!("a daemon message of {} bytes is too large", bytes.len()))?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Reads a message written by [`write_json_message`].
pub async fn read_json_message<S: AsyncRead + Unpin, T: DeserializeOwned>(
    stream: &mut S,
) -> Result<T> {
    let mut len_bytes = [0u8; MESSAGE_LEN_SIZE];
    stream.read_exact(&mut len_bytes).await?;
    let len = MessageLen::from_le_bytes(len_bytes);
    anyhow::ensure!(
        len <= MAX_DAEMON_JSON_MESSAGE_LEN,
        "a daemon message claims to be {len} bytes long, which is not an introduction"
    );
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("decoding a daemon message")
}

/// What a frame on an accepted daemon connection carries.
///
/// One socket replaces the three (`stdin` / `stdout` / `stderr`) a
/// per-project server used: those were paired by the order in which they were
/// accepted, which cannot tell two proxies apart once several share a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonFrameKind {
    /// An encoded [`Envelope`], in either direction.
    Envelope,
    /// Log output for the proxy to put on its stderr, where the client reads
    /// the server's log from. Daemon to proxy only.
    Log,
}

impl DaemonFrameKind {
    fn to_byte(self) -> u8 {
        match self {
            DaemonFrameKind::Envelope => 0,
            DaemonFrameKind::Log => 1,
        }
    }

    fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            0 => Ok(DaemonFrameKind::Envelope),
            1 => Ok(DaemonFrameKind::Log),
            other => anyhow::bail!("unknown daemon frame kind {other}"),
        }
    }
}

/// Writes one frame: its kind, its length, then `payload`. Does not flush.
pub async fn write_daemon_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    kind: DaemonFrameKind,
    payload: &[u8],
) -> Result<()> {
    let len = MessageLen::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_DAEMON_FRAME_LEN)
        .with_context(|| format!("a frame of {} bytes is too large", payload.len()))?;
    stream.write_all(&[kind.to_byte()]).await?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

/// Reads one frame into `buffer`, returning what it carries.
pub async fn read_daemon_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<DaemonFrameKind> {
    let mut kind = [0u8; 1];
    stream.read_exact(&mut kind).await?;
    let kind = DaemonFrameKind::from_byte(kind[0])?;
    let mut len_bytes = [0u8; MESSAGE_LEN_SIZE];
    stream.read_exact(&mut len_bytes).await?;
    let len = MessageLen::from_le_bytes(len_bytes);
    // Checked before anything is allocated for it.
    anyhow::ensure!(
        len <= MAX_DAEMON_FRAME_LEN,
        "a frame claims to be {len} bytes long, more than the {MAX_DAEMON_FRAME_LEN} allowed"
    );
    buffer.resize(len as usize, 0);
    stream.read_exact(buffer).await?;
    Ok(kind)
}

/// Encodes `envelope` into `buffer` and writes it as an envelope frame.
pub async fn write_envelope_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    envelope: Envelope,
) -> Result<()> {
    buffer.clear();
    buffer.reserve(envelope.encoded_len());
    envelope.encode(buffer)?;
    write_daemon_frame(stream, DaemonFrameKind::Envelope, buffer).await
}

pub fn decode_envelope(bytes: &[u8]) -> Result<Envelope> {
    Ok(Envelope::decode(bytes)?)
}

/// The client instance id carried at the front of a connection identifier,
/// or `""` for an identifier that has none.
pub fn client_id_of_identifier(identifier: &str) -> &str {
    identifier
        .split_once(CLIENT_ID_SEPARATOR)
        .map_or("", |(client_id, _)| client_id)
}

/// What decides which daemon a proxy talks to.
///
/// The executable's size and modification time are in here because a
/// development build is re-uploaded under the same name and version whenever
/// its code changes: without them, a proxy of the new build would hand its
/// session to a daemon still running the old code.
pub struct DaemonKeyParts<'a> {
    pub release_channel: &'a str,
    pub version: &'a str,
    pub executable_size: u64,
    pub executable_modified: Option<Duration>,
    pub client_id: &'a str,
    /// Which machine this is. Hosts that share a home directory — over NFS,
    /// as build farms often do — would otherwise start and kill one another's
    /// daemon in the same directory.
    pub host: &'a str,
}

/// Names a daemon: eight hex digits of a SHA-256 over [`DaemonKeyParts`].
/// Short, because it ends up in a unix socket path, and those are limited to
/// about a hundred bytes.
pub fn daemon_key(parts: &DaemonKeyParts) -> String {
    let modified = parts.executable_modified.map_or(String::new(), |modified| {
        format!("{}.{:09}", modified.as_secs(), modified.subsec_nanos())
    });
    let size = parts.executable_size.to_string();
    let mut hasher = Sha256::new();
    for field in [
        parts.release_channel,
        parts.version,
        size.as_str(),
        modified.as_str(),
        parts.client_id,
        parts.host,
    ] {
        // Length-prefixed, so that moving a character from one field into its
        // neighbour cannot produce the same key.
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hex::encode(&hasher.finalize()[..4])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::proto::{self, EnvelopedMessage as _};

    fn key_parts<'a>(client_id: &'a str, executable_size: u64) -> DaemonKeyParts<'a> {
        DaemonKeyParts {
            release_channel: "dev",
            version: "1.17.2+abcdef",
            executable_size,
            executable_modified: Some(Duration::new(1_760_000_000, 5)),
            client_id,
            host: "build-01\n0123456789abcdef",
        }
    }

    #[test]
    fn the_daemon_key_is_eight_hex_digits_and_changes_with_every_part() {
        let key = daemon_key(&key_parts("client-a", 100));
        assert_eq!(key.len(), 8);
        assert!(key.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(key, daemon_key(&key_parts("client-a", 100)));

        assert_ne!(key, daemon_key(&key_parts("client-b", 100)));
        assert_ne!(key, daemon_key(&key_parts("client-a", 101)));
        let mut rebuilt = key_parts("client-a", 100);
        rebuilt.executable_modified = Some(Duration::new(1_760_000_000, 6));
        assert_ne!(key, daemon_key(&rebuilt));
        let mut other_channel = key_parts("client-a", 100);
        other_channel.release_channel = "stable";
        assert_ne!(key, daemon_key(&other_channel));
        let mut other_host = key_parts("client-a", 100);
        other_host.host = "build-02\n0123456789abcdef";
        assert_ne!(
            key,
            daemon_key(&other_host),
            "hosts sharing a home directory must not share a daemon directory"
        );
    }

    #[test]
    fn the_client_id_is_read_from_the_front_of_the_identifier() {
        assert_eq!(client_id_of_identifier("0a1b2c3d.workspace-5"), "0a1b2c3d");
        assert_eq!(client_id_of_identifier("0a1b2c3d.dev-setup-1"), "0a1b2c3d");
        // What a client from before ADR-F45 sends.
        assert_eq!(client_id_of_identifier("workspace-5"), "");
        assert_eq!(client_id_of_identifier("dev-workspace-5"), "");
    }

    #[test]
    fn an_oversized_frame_is_refused_before_anything_is_allocated_for_it() {
        smol::block_on(async {
            let mut wire = vec![0u8];
            wire.extend_from_slice(&(MAX_DAEMON_FRAME_LEN + 1).to_le_bytes());
            let mut reader = futures::io::Cursor::new(wire);
            let mut buffer = Vec::new();
            assert!(read_daemon_frame(&mut reader, &mut buffer).await.is_err());
            assert_eq!(buffer.capacity(), 0);
        });
    }

    #[test]
    fn a_frame_cut_short_is_an_error() {
        smol::block_on(async {
            let mut wire = vec![0u8];
            wire.extend_from_slice(&10u32.to_le_bytes());
            wire.extend_from_slice(b"abc");
            let mut reader = futures::io::Cursor::new(wire);
            let mut buffer = Vec::new();
            assert!(read_daemon_frame(&mut reader, &mut buffer).await.is_err());
        });
    }

    #[test]
    fn an_introduction_without_an_agent_socket_still_reads() {
        let hello: DaemonHello = serde_json::from_str(
            r#"{"protocol_version":1,"identifier":"workspace-5","reconnect":false}"#,
        )
        .expect("reading an introduction without the optional fields");
        assert_eq!(hello.ssh_auth_sock, None);
    }

    #[test]
    fn frames_keep_their_kind_and_payload() {
        smol::block_on(async {
            let envelope = proto::Ping {}.into_envelope(7, None, None);
            let mut wire = Vec::new();
            let mut buffer = Vec::new();
            write_envelope_frame(&mut wire, &mut buffer, envelope.clone())
                .await
                .expect("writing an envelope frame");
            write_daemon_frame(&mut wire, DaemonFrameKind::Log, b"{\"level\":1}\n")
                .await
                .expect("writing a log frame");

            let mut reader = futures::io::Cursor::new(wire);
            let kind = read_daemon_frame(&mut reader, &mut buffer)
                .await
                .expect("reading the envelope frame");
            assert_eq!(kind, DaemonFrameKind::Envelope);
            assert_eq!(
                decode_envelope(&buffer).expect("decoding the envelope"),
                envelope
            );
            let kind = read_daemon_frame(&mut reader, &mut buffer)
                .await
                .expect("reading the log frame");
            assert_eq!(kind, DaemonFrameKind::Log);
            assert_eq!(buffer, b"{\"level\":1}\n");
        });
    }

    #[test]
    fn an_unknown_frame_kind_is_an_error() {
        smol::block_on(async {
            let mut reader = futures::io::Cursor::new(vec![9u8, 0, 0, 0, 0]);
            let mut buffer = Vec::new();
            assert!(read_daemon_frame(&mut reader, &mut buffer).await.is_err());
        });
    }

    #[test]
    fn the_introduction_round_trips() {
        smol::block_on(async {
            let hello = DaemonHello {
                protocol_version: DAEMON_PROTOCOL_VERSION,
                identifier: "0a1b2c3d.workspace-5".to_string(),
                reconnect: true,
                ssh_auth_sock: Some("/tmp/ssh-XXXXabcd/agent.1234".to_string()),
            };
            let reply = DaemonHelloReply {
                outcome: DaemonHelloOutcome::SessionNotFound,
                message: None,
            };
            let mut wire = Vec::new();
            write_json_message(&mut wire, &hello)
                .await
                .expect("writing the hello");
            write_json_message(&mut wire, &reply)
                .await
                .expect("writing the reply");

            let mut reader = futures::io::Cursor::new(wire);
            let read_hello: DaemonHello = read_json_message(&mut reader)
                .await
                .expect("reading the hello");
            let read_reply: DaemonHelloReply = read_json_message(&mut reader)
                .await
                .expect("reading the reply");
            assert_eq!(read_hello, hello);
            assert_eq!(read_reply, reply);
        });
    }

    #[test]
    fn an_oversized_introduction_is_refused_before_it_is_read() {
        smol::block_on(async {
            let mut wire = (MAX_DAEMON_JSON_MESSAGE_LEN + 1).to_le_bytes().to_vec();
            wire.extend_from_slice(b"{}");
            let mut reader = futures::io::Cursor::new(wire);
            let result: Result<DaemonHello> = read_json_message(&mut reader).await;
            assert!(result.is_err());
        });
    }
}
