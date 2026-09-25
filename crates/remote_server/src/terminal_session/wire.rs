//! What a holder and the processes that connect to it say to each other: a
//! fixed preamble from each side, then length-prefixed frames.

use anyhow::{Context as _, Result, anyhow, bail};

/// Bumped whenever anything after the preamble changes. An attach that meets
/// a holder of another version gives up instead of guessing (a Zed update can
/// bring a new attach to a holder started by the previous build).
pub(crate) const PROTOCOL_VERSION: u32 = 1;

const MAGIC: [u8; 4] = *b"ZRTS";
pub(crate) const PREAMBLE_LEN: usize = 9;
const FRAME_HEADER_LEN: usize = 5;
pub(crate) const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
/// How much output one frame carries at most.
pub(crate) const OUTPUT_CHUNK: usize = 64 * 1024;

/// Who sent a preamble. The preamble's layout and the meaning of [`Role::Kill`]
/// must never change: a kill has to reach holders of every version, since a
/// tab closed after a Zed update still has to end the shell an older build
/// started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Holder = 0,
    Attach = 1,
    Kill = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Preamble {
    pub version: u32,
    pub role: Role,
}

pub(crate) fn encode_preamble(role: Role) -> [u8; PREAMBLE_LEN] {
    let mut bytes = [0; PREAMBLE_LEN];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4..8].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    bytes[8] = role as u8;
    bytes
}

fn decode_preamble(bytes: &[u8; PREAMBLE_LEN]) -> Result<Preamble> {
    if bytes[..4] != MAGIC {
        bail!("the other side is not a terminal session holder");
    }
    let mut version = [0; 4];
    version.copy_from_slice(&bytes[4..8]);
    let role = match bytes[8] {
        0 => Role::Holder,
        1 => Role::Attach,
        2 => Role::Kill,
        other => bail!("unknown terminal session role {other}"),
    };
    Ok(Preamble {
        version: u32::from_be_bytes(version),
        role,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

impl WindowSize {
    pub const FALLBACK: WindowSize = WindowSize { rows: 24, cols: 80 };

    fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.rows.to_be_bytes());
        out.extend_from_slice(&self.cols.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let [rows_high, rows_low, cols_high, cols_low] = bytes else {
            bail!("a window size is 4 bytes, not {}", bytes.len());
        };
        Ok(Self {
            rows: u16::from_be_bytes([*rows_high, *rows_low]),
            cols: u16::from_be_bytes([*cols_high, *cols_low]),
        })
    }
}

/// How the shell ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellStatus {
    Exited(i32),
    Signaled(i32),
}

impl ShellStatus {
    /// The code the user is told about: the shell's own, or 128 + the signal
    /// as shells report it.
    pub fn displayed_code(self) -> i32 {
        match self {
            ShellStatus::Exited(code) => code,
            ShellStatus::Signaled(signal) => 128 + signal,
        }
    }

    /// The attach's exit code. 255 becomes 254 because Zed reads 255 as "the
    /// SSH connection dropped" and restarts the terminal after reconnecting,
    /// and 253 becomes 254 because Zed reads 253 as "another attach took the
    /// shell over" and keeps the tab open.
    pub fn attach_exit_code(self) -> i32 {
        match self.displayed_code() & 0xff {
            253 | 255 => 254,
            code => code,
        }
    }
}

/// From an attach to its holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToHolder {
    Hello { size: WindowSize, id: String },
    Input(Vec<u8>),
    Resize(WindowSize),
}

/// From a holder to its attach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToAttach {
    Replay(Vec<u8>),
    ReplayEnd,
    Output(Vec<u8>),
    ShellExited {
        status: ShellStatus,
        while_detached: bool,
    },
    TakenOver,
    Error(String),
}

fn encode_frame(kind: u8, payload: &[u8], out: &mut Vec<u8>) {
    debug_assert!(payload.len() <= MAX_FRAME_PAYLOAD);
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

impl ToHolder {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ToHolder::Hello { size, id } => {
                let mut payload = Vec::with_capacity(4 + id.len());
                size.encode(&mut payload);
                payload.extend_from_slice(id.as_bytes());
                encode_frame(1, &payload, out);
            }
            ToHolder::Input(bytes) => {
                for chunk in bytes.chunks(OUTPUT_CHUNK) {
                    encode_frame(2, chunk, out);
                }
            }
            ToHolder::Resize(size) => {
                let mut payload = Vec::with_capacity(4);
                size.encode(&mut payload);
                encode_frame(3, &payload, out);
            }
        }
    }

    fn decode(kind: u8, payload: Vec<u8>) -> Result<Self> {
        Ok(match kind {
            1 => {
                let size = WindowSize::decode(payload.get(..4).context("a short hello")?)?;
                let id = String::from_utf8(payload[4..].to_vec()).context("a hello's id")?;
                ToHolder::Hello { size, id }
            }
            2 => ToHolder::Input(payload),
            3 => ToHolder::Resize(WindowSize::decode(&payload)?),
            other => bail!("unknown frame {other} for a holder"),
        })
    }
}

impl ToAttach {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ToAttach::Replay(bytes) => {
                for chunk in bytes.chunks(OUTPUT_CHUNK) {
                    encode_frame(1, chunk, out);
                }
            }
            ToAttach::ReplayEnd => encode_frame(2, &[], out),
            ToAttach::Output(bytes) => {
                for chunk in bytes.chunks(OUTPUT_CHUNK) {
                    encode_frame(3, chunk, out);
                }
            }
            ToAttach::ShellExited {
                status,
                while_detached,
            } => {
                let (how, value) = match status {
                    ShellStatus::Exited(code) => (0, *code),
                    ShellStatus::Signaled(signal) => (1, *signal),
                };
                let mut payload = vec![how];
                payload.extend_from_slice(&value.to_be_bytes());
                payload.push(u8::from(*while_detached));
                encode_frame(4, &payload, out);
            }
            ToAttach::TakenOver => encode_frame(5, &[], out),
            ToAttach::Error(message) => {
                let message = message.as_bytes();
                encode_frame(6, &message[..message.len().min(4096)], out);
            }
        }
    }

    fn decode(kind: u8, payload: Vec<u8>) -> Result<Self> {
        Ok(match kind {
            1 => ToAttach::Replay(payload),
            2 => ToAttach::ReplayEnd,
            3 => ToAttach::Output(payload),
            4 => {
                let [how, value @ .., while_detached] = payload.as_slice() else {
                    bail!("an empty shell exit frame");
                };
                let value = i32::from_be_bytes(
                    value
                        .try_into()
                        .map_err(|_| anyhow!("a shell exit frame of {} bytes", payload.len()))?,
                );
                let status = match how {
                    0 => ShellStatus::Exited(value),
                    1 => ShellStatus::Signaled(value),
                    other => bail!("unknown shell exit kind {other}"),
                };
                ToAttach::ShellExited {
                    status,
                    while_detached: *while_detached != 0,
                }
            }
            5 => ToAttach::TakenOver,
            6 => ToAttach::Error(String::from_utf8_lossy(&payload).into_owned()),
            other => bail!("unknown frame {other} for an attach"),
        })
    }
}

/// Collects bytes read from a connection and hands out the preamble and then
/// whole frames.
#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    #[cfg(test)]
    fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn take_preamble(&mut self) -> Result<Option<Preamble>> {
        let Some(bytes) = self.buffer.first_chunk::<PREAMBLE_LEN>() else {
            return Ok(None);
        };
        let preamble = decode_preamble(bytes)?;
        self.buffer.drain(..PREAMBLE_LEN);
        Ok(Some(preamble))
    }

    fn take_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        let Some(header) = self.buffer.first_chunk::<FRAME_HEADER_LEN>() else {
            return Ok(None);
        };
        let kind = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if len > MAX_FRAME_PAYLOAD {
            bail!("a frame of {len} bytes is over the limit");
        }
        if self.buffer.len() < FRAME_HEADER_LEN + len {
            return Ok(None);
        }
        let payload = self.buffer[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len].to_vec();
        self.buffer.drain(..FRAME_HEADER_LEN + len);
        Ok(Some((kind, payload)))
    }

    pub fn take_to_holder(&mut self) -> Result<Option<ToHolder>> {
        self.take_frame()?
            .map(|(kind, payload)| ToHolder::decode(kind, payload))
            .transpose()
    }

    pub fn take_to_attach(&mut self) -> Result<Option<ToAttach>> {
        self.take_frame()?
            .map(|(kind, payload)| ToAttach::decode(kind, payload))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all_to_attach(bytes: &[u8], step: usize) -> Vec<ToAttach> {
        let mut decoder = Decoder::default();
        let mut messages = Vec::new();
        for chunk in bytes.chunks(step) {
            decoder.extend(chunk);
            while let Some(message) = decoder.take_to_attach().expect("a valid frame") {
                messages.push(message);
            }
        }
        assert_eq!(decoder.buffered_len(), 0);
        messages
    }

    #[test]
    fn frames_survive_any_split() {
        let messages = vec![
            ToAttach::Replay(b"old output".to_vec()),
            ToAttach::ReplayEnd,
            ToAttach::Output(vec![0, 1, 2, 0x1b, 0xff]),
            ToAttach::ShellExited {
                status: ShellStatus::Exited(-3),
                while_detached: true,
            },
            ToAttach::ShellExited {
                status: ShellStatus::Signaled(9),
                while_detached: false,
            },
            ToAttach::TakenOver,
            ToAttach::Error("もう居ません".to_string()),
        ];
        let mut bytes = Vec::new();
        for message in &messages {
            message.encode(&mut bytes);
        }
        for step in [1, 2, 3, 7, bytes.len()] {
            assert_eq!(decode_all_to_attach(&bytes, step), messages);
        }

        let messages = vec![
            ToHolder::Hello {
                size: WindowSize {
                    rows: 50,
                    cols: 200,
                },
                id: "abc-DEF_1".to_string(),
            },
            ToHolder::Input(b"ls\r".to_vec()),
            ToHolder::Resize(WindowSize {
                rows: 1,
                cols: u16::MAX,
            }),
        ];
        let mut bytes = Vec::new();
        for message in &messages {
            message.encode(&mut bytes);
        }
        let mut decoder = Decoder::default();
        let mut decoded = Vec::new();
        for byte in bytes {
            decoder.extend(&[byte]);
            while let Some(message) = decoder.take_to_holder().expect("a valid frame") {
                decoded.push(message);
            }
        }
        assert_eq!(decoded, messages);
    }

    #[test]
    fn large_output_is_split_into_frames() {
        let output = vec![b'x'; OUTPUT_CHUNK * 2 + 1];
        let mut bytes = Vec::new();
        ToAttach::Replay(output.clone()).encode(&mut bytes);
        let messages = decode_all_to_attach(&bytes, 4096);
        assert_eq!(messages.len(), 3);
        let joined: Vec<u8> = messages
            .into_iter()
            .flat_map(|message| match message {
                ToAttach::Replay(bytes) => bytes,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(joined, output);
    }

    #[test]
    fn oversized_and_unknown_frames_are_refused() {
        let mut decoder = Decoder::default();
        decoder.extend(&[3]);
        decoder.extend(&((MAX_FRAME_PAYLOAD as u32) + 1).to_be_bytes());
        assert!(decoder.take_to_attach().is_err());

        let mut decoder = Decoder::default();
        decoder.extend(&[99, 0, 0, 0, 0]);
        assert!(decoder.take_to_attach().is_err());
        let mut decoder = Decoder::default();
        decoder.extend(&[99, 0, 0, 0, 0]);
        assert!(decoder.take_to_holder().is_err());
    }

    #[test]
    fn the_preamble_comes_first() {
        let mut decoder = Decoder::default();
        let preamble = encode_preamble(Role::Kill);
        decoder.extend(&preamble[..5]);
        assert_eq!(decoder.take_preamble().expect("so far valid"), None);
        decoder.extend(&preamble[5..]);
        ToHolder::Resize(WindowSize::FALLBACK).encode(&mut decoder.buffer);
        assert_eq!(
            decoder.take_preamble().expect("valid"),
            Some(Preamble {
                version: PROTOCOL_VERSION,
                role: Role::Kill
            })
        );
        assert_eq!(
            decoder.take_to_holder().expect("valid"),
            Some(ToHolder::Resize(WindowSize::FALLBACK))
        );

        // The layout every version keeps (see `Role`).
        assert_eq!(
            encode_preamble(Role::Kill),
            [b'Z', b'R', b'T', b'S', 0, 0, 0, 1, 2]
        );

        let mut decoder = Decoder::default();
        decoder.extend(b"SSH-2.0-OpenSSH");
        assert!(decoder.take_preamble().is_err());
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(ShellStatus::Exited(0).attach_exit_code(), 0);
        assert_eq!(ShellStatus::Exited(7).attach_exit_code(), 7);
        assert_eq!(ShellStatus::Exited(254).attach_exit_code(), 254);
        assert_eq!(ShellStatus::Exited(255).attach_exit_code(), 254);
        assert_eq!(ShellStatus::Exited(253).attach_exit_code(), 254);
        assert_eq!(ShellStatus::Signaled(125).attach_exit_code(), 254);
        assert_eq!(ShellStatus::Exited(252).attach_exit_code(), 252);
        assert_eq!(ShellStatus::Exited(255).displayed_code(), 255);
        assert_eq!(ShellStatus::Signaled(15).attach_exit_code(), 143);
        assert_eq!(ShellStatus::Signaled(9).attach_exit_code(), 137);
        assert_eq!(ShellStatus::Signaled(127).attach_exit_code(), 254);
    }
}
