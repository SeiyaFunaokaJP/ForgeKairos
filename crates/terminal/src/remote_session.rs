//! Persistent remote terminal sessions.
//!
//! A remote shell normally lives and dies with the local `ssh` process that
//! carries it, so a dropped connection takes the scrollback and any running
//! command with it. With persistence on, the command `ssh` runs is the remote
//! server's `terminal-session attach`, which hands the shell to a holder
//! process on the host that outlives the connection. Every attach replays what
//! the shell printed so far before the live output, framed by two OSC 7719
//! markers that carry a nonce the client picked for that attach:
//!
//! ```text
//! ESC ] 7719 ; replay-begin ; <nonce> BEL   <replayed output>   ESC ] 7719 ; replay-end ; <nonce> BEL   <live output>
//! ```
//!
//! [`ReplayMarkerFilter`] takes the markers out of the PTY stream before the
//! emulator or the session log sees them, and keeps the replayed part out of
//! the session log, which already holds it from the attach that first printed
//! it.

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// What `terminal-session attach` exits with when another attach took the
/// shell over. The server has already said so in the grid.
pub const TAKEN_OVER_EXIT_CODE: i32 = 253;

/// The persistent session a remote terminal is attached to, and the nonce of
/// the one attach that terminal performed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTerminalSession {
    pub id: String,
    pub nonce: String,
    /// Whether the attach asked for a session an earlier attach had (after a
    /// dropped connection or a restart of Zed), rather than a new one.
    pub resume: bool,
}

/// Why the attach of a persistent session ended before it reached the
/// holder, as its exit code tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteSessionFailure {
    /// The remote server binary does not know `terminal-session` (the usage
    /// error of its argument parser, 2) or is not there (127), so no shell
    /// on that server can have a session.
    Unsupported,
    /// The binary knows sessions but could not reach a holder for this one
    /// (1: the holder did not start, or it is of another version). Only this
    /// session is given up.
    Unreachable,
}

impl RemoteSessionFailure {
    /// The failure an attach that never connected exited with. `None` for a
    /// clean exit, which is the user's `exit` rather than a failure.
    pub fn from_exit_code(code: i32) -> Option<Self> {
        match code {
            0 => None,
            2 | 127 => Some(Self::Unsupported),
            _ => Some(Self::Unreachable),
        }
    }
}

/// The command `ssh` runs to attach to — or start — the session, in place of
/// `program args…`. The holder only starts `program` when no shell of that id
/// is alive, and it inherits the directory and environment this command runs
/// in, so the `cd … && exec env …` prefix `ssh` builds around it keeps working.
///
/// `binary` is the remote server's path relative to the remote home, which is
/// where the command line `ssh` builds starts from when it is given no
/// directory of its own.
pub fn persistent_session_command(
    binary: &str,
    session: &RemoteTerminalSession,
    detached_timeout_secs: u64,
    program: String,
    args: Vec<String>,
) -> (String, Vec<String>) {
    let mut wrapped_args = vec![
        "terminal-session".to_string(),
        "attach".to_string(),
        "--id".to_string(),
        session.id.clone(),
        "--nonce".to_string(),
        session.nonce.clone(),
    ];
    if session.resume {
        wrapped_args.push("--resume".to_string());
    }
    wrapped_args.extend([
        "--detached-timeout-secs".to_string(),
        detached_timeout_secs.to_string(),
        "--".to_string(),
        program,
    ]);
    wrapped_args.extend(args);
    (binary.to_string(), wrapped_args)
}

/// Bytes a [`ReplayMarkerFilter`] let through, with the ranges of them that
/// are live output rather than replay.
#[derive(Default)]
pub(crate) struct FilteredOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) live: Vec<Range<usize>>,
}

impl FilteredOutput {
    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.live.clear();
    }

    fn push(&mut self, bytes: &[u8], live: bool) {
        if bytes.is_empty() {
            return;
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        let end = self.bytes.len();
        if !live {
            return;
        }
        match self.live.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => self.live.push(start..end),
        }
    }
}

/// Removes this attach's replay markers from a PTY byte stream that arrives in
/// arbitrary chunks, and tells which of the remaining bytes are replay.
///
/// Only markers carrying this attach's nonce are removed: anything else —
/// another OSC 7719, one with a different nonce, or one whose nonce merely
/// starts with ours — passes through untouched, so a program in the shell
/// cannot fake the end of a replay without knowing the nonce.
pub(crate) struct ReplayMarkerFilter {
    begin_marker: Vec<u8>,
    end_marker: Vec<u8>,
    /// Bytes that so far match the start of a marker, held back until they
    /// either complete one or stop matching. Only the first byte of a marker is
    /// `ESC`, so a mismatch never hides the start of the next candidate
    /// anywhere but in the byte that caused it.
    candidate: Vec<u8>,
    replaying: bool,
    attached: Arc<AtomicBool>,
}

impl ReplayMarkerFilter {
    pub(crate) fn new(nonce: &str, attached: Arc<AtomicBool>) -> Self {
        Self {
            begin_marker: marker("replay-begin", nonce),
            end_marker: marker("replay-end", nonce),
            candidate: Vec::new(),
            replaying: false,
            attached,
        }
    }

    /// How many bytes are held back, and so will come out in front of the
    /// next chunk's own.
    pub(crate) fn pending_len(&self) -> usize {
        self.candidate.len()
    }

    pub(crate) fn filter(&mut self, input: &[u8], output: &mut FilteredOutput) {
        let mut rest = input;
        while !rest.is_empty() {
            if self.candidate.is_empty() {
                let Some(escape) = rest.iter().position(|byte| *byte == ESC) else {
                    output.push(rest, !self.replaying);
                    return;
                };
                output.push(&rest[..escape], !self.replaying);
                self.candidate.push(ESC);
                rest = &rest[escape + 1..];
                continue;
            }

            let Some((&byte, tail)) = rest.split_first() else {
                return;
            };
            rest = tail;
            self.candidate.push(byte);
            if self.candidate == self.begin_marker {
                self.candidate.clear();
                self.replaying = true;
                self.attached.store(true, Ordering::Release);
            } else if self.candidate == self.end_marker {
                self.candidate.clear();
                self.replaying = false;
            } else if !self.begin_marker.starts_with(&self.candidate)
                && !self.end_marker.starts_with(&self.candidate)
            {
                self.candidate.pop();
                output.push(&self.candidate, !self.replaying);
                self.candidate.clear();
                if byte == ESC {
                    self.candidate.push(ESC);
                } else {
                    output.push(&[byte], !self.replaying);
                }
            }
        }
    }
}

fn marker(kind: &str, nonce: &str) -> Vec<u8> {
    let mut marker = vec![ESC];
    marker.extend_from_slice(format!("]7719;{kind};{nonce}").as_bytes());
    marker.push(BEL);
    marker
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "a1b2c3d4e5f6";

    fn filter_chunks(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>, Arc<AtomicBool>) {
        let attached = Arc::new(AtomicBool::new(false));
        let mut filter = ReplayMarkerFilter::new(NONCE, attached.clone());
        let mut shown = Vec::new();
        let mut live = Vec::new();
        let mut output = FilteredOutput::default();
        for chunk in chunks {
            output.clear();
            filter.filter(chunk, &mut output);
            for range in &output.live {
                live.extend_from_slice(&output.bytes[range.clone()]);
            }
            shown.extend_from_slice(&output.bytes);
        }
        (shown, live, attached)
    }

    fn stream() -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\x1b]7719;replay-begin;a1b2c3d4e5f6\x07");
        stream.extend_from_slice(b"old line\r\n\x1b[31mred\x1b[0m\r\n$ ");
        stream.extend_from_slice(b"\x1b]7719;replay-end;a1b2c3d4e5f6\x07");
        stream.extend_from_slice(b"ls\r\nfile\r\n");
        stream
    }

    #[test]
    fn markers_are_removed_and_only_live_output_is_marked_live() {
        let (shown, live, attached) = filter_chunks(&[&stream()]);
        assert_eq!(
            shown,
            b"old line\r\n\x1b[31mred\x1b[0m\r\n$ ls\r\nfile\r\n".to_vec()
        );
        assert_eq!(live, b"ls\r\nfile\r\n".to_vec());
        assert!(attached.load(Ordering::Acquire));
    }

    #[test]
    fn markers_split_at_every_byte_are_still_removed() {
        let stream = stream();
        let chunks = stream.chunks(1).collect::<Vec<_>>();
        let (shown, live, attached) = filter_chunks(&chunks);
        assert_eq!(
            shown,
            b"old line\r\n\x1b[31mred\x1b[0m\r\n$ ls\r\nfile\r\n".to_vec()
        );
        assert_eq!(live, b"ls\r\nfile\r\n".to_vec());
        assert!(attached.load(Ordering::Acquire));
    }

    #[test]
    fn markers_of_another_nonce_and_other_oscs_pass_through() {
        let foreign = b"\x1b]7719;replay-begin;ffffffffffff\x07\x1b]7719;other\x07\x1b]7;file://h/tmp\x07\x1b]7719;replay-end;a1b2c3d4e5f6X\x07\x1b\x1b[0mtext";
        for chunk_size in [1, 3, foreign.len()] {
            let chunks = foreign.chunks(chunk_size).collect::<Vec<_>>();
            let (shown, live, attached) = filter_chunks(&chunks);
            assert_eq!(shown, foreign.to_vec(), "chunk size {chunk_size}");
            assert_eq!(live, foreign.to_vec(), "chunk size {chunk_size}");
            assert!(!attached.load(Ordering::Acquire));
        }
    }

    #[test]
    fn a_new_attach_with_nothing_to_replay_still_counts_as_attached() {
        let (shown, live, attached) = filter_chunks(&[
            b"\x1b]7719;replay-begin;a1b2c3d4e5f6\x07\x1b]7719;replay-end;a1b2c3d4e5f6\x07",
            b"$ ",
        ]);
        assert_eq!(shown, b"$ ".to_vec());
        assert_eq!(live, b"$ ".to_vec());
        assert!(attached.load(Ordering::Acquire));
    }

    #[test]
    fn the_session_command_wraps_the_shell_after_a_double_dash() {
        let mut session = RemoteTerminalSession {
            id: "0f8e5d7c-2a4b-4c6d-8e9f-001122334455".to_string(),
            nonce: "abcdef0123456789".to_string(),
            resume: true,
        };
        let (program, args) = persistent_session_command(
            ".zed_server/zed-remote-server-dev",
            &session,
            86400,
            "/bin/bash".to_string(),
            vec!["-l".to_string()],
        );
        assert_eq!(program, ".zed_server/zed-remote-server-dev");
        assert_eq!(
            args,
            [
                "terminal-session",
                "attach",
                "--id",
                "0f8e5d7c-2a4b-4c6d-8e9f-001122334455",
                "--nonce",
                "abcdef0123456789",
                "--resume",
                "--detached-timeout-secs",
                "86400",
                "--",
                "/bin/bash",
                "-l",
            ]
        );

        session.resume = false;
        let (_, args) = persistent_session_command(
            ".zed_server/zed-remote-server-dev",
            &session,
            0,
            "/bin/bash".to_string(),
            vec!["-l".to_string()],
        );
        assert!(!args.contains(&"--resume".to_string()));
        assert!(args.contains(&"0".to_string()));
    }
}
