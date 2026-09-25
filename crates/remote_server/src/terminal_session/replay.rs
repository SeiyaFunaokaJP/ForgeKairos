//! The output a holder remembers, and what of it is safe to show a new
//! terminal again.

use std::collections::VecDeque;

use super::TRUNCATED_REPLAY_NOTICE;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const ENQ: u8 = 0x05;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// Longer control sequences are passed through without being looked at: no
/// query is anywhere near this long, and holding on to them would let one
/// unterminated sequence buffer the rest of the output.
const MAX_CONTROL_SEQUENCE_LEN: usize = 4 * 1024;
const MAX_STRING_SEQUENCE_LEN: usize = 64 * 1024;

const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";

/// The tail of a shell's output, up to a byte budget.
pub(crate) struct OutputHistory {
    buffer: VecDeque<u8>,
    capacity: usize,
    dropped: bool,
    screen: ScreenModeTracker,
}

pub(crate) struct Replay {
    pub bytes: Vec<u8>,
    /// Whether a full-screen program is running and has to be made to draw
    /// itself again: its screen is not in the replay once the start of it
    /// has been dropped, and a fresh terminal would show it wrongly anyway.
    pub needs_redraw: bool,
}

impl OutputHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            capacity,
            dropped: false,
            screen: ScreenModeTracker::default(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        if bytes.len() >= self.capacity {
            self.dropped |= !self.buffer.is_empty() || bytes.len() > self.capacity;
            self.buffer.clear();
            self.buffer.extend(&bytes[bytes.len() - self.capacity..]);
            return;
        }
        self.buffer.extend(bytes);
        let excess = self.buffer.len().saturating_sub(self.capacity);
        if excess > 0 {
            self.buffer.drain(..excess);
            self.dropped = true;
        }
    }

    pub fn replay(&mut self) -> Replay {
        let retained = self.buffer.make_contiguous();
        let mut bytes = Vec::with_capacity(retained.len() + TRUNCATED_REPLAY_NOTICE.len());
        let retained = if self.dropped {
            bytes.extend_from_slice(TRUNCATED_REPLAY_NOTICE.as_bytes());
            &retained[replay_start(retained)..]
        } else {
            &retained[..]
        };
        sanitize_into(retained, &mut bytes);
        let alternate = self.screen.alternate;
        if alternate && last_alternate_screen_switch(retained) != Some(true) {
            bytes.extend_from_slice(ENTER_ALTERNATE_SCREEN);
        }
        Replay {
            bytes,
            needs_redraw: alternate,
        }
    }
}

/// Where a replay whose beginning was dropped starts: right after the first
/// line break, so that it neither starts halfway through a line nor inside
/// an escape sequence (none of which holds a line break).
fn replay_start(retained: &[u8]) -> usize {
    retained
        .iter()
        .position(|&byte| byte == b'\n')
        .or_else(|| retained.iter().position(|&byte| byte == b'\r'))
        .map_or(retained.len(), |position| position + 1)
}

/// Removes what would make a terminal answer: shown again, those requests
/// would have the new terminal type its answers into whatever program is
/// running now.
#[cfg(test)]
fn sanitize(bytes: &[u8]) -> Vec<u8> {
    let mut sanitized = Vec::with_capacity(bytes.len());
    sanitize_into(bytes, &mut sanitized);
    sanitized
}

fn sanitize_into(bytes: &[u8], out: &mut Vec<u8>) {
    let mut keep = |piece: Piece<'_>| match piece {
        Piece::Bytes(bytes) => out.extend(bytes.iter().filter(|&&byte| byte != ENQ)),
        Piece::Sequence(sequence) => {
            if !is_terminal_query(sequence) {
                out.extend_from_slice(sequence);
            }
        }
    };
    let mut scanner = Scanner::default();
    scanner.feed(bytes, &mut keep);
    scanner.finish(&mut keep);
}

fn last_alternate_screen_switch(bytes: &[u8]) -> Option<bool> {
    let mut last = None;
    let mut scanner = Scanner::default();
    scanner.feed(bytes, &mut |piece| {
        if let Piece::Sequence(sequence) = piece
            && let Some(alternate) = alternate_screen_switch(sequence)
        {
            last = Some(alternate);
        }
    });
    last
}

#[derive(Default)]
struct ScreenModeTracker {
    scanner: Scanner,
    alternate: bool,
}

impl ScreenModeTracker {
    fn feed(&mut self, bytes: &[u8]) {
        let alternate = &mut self.alternate;
        self.scanner.feed(bytes, &mut |piece| {
            if let Piece::Sequence(sequence) = piece
                && let Some(switch) = alternate_screen_switch(sequence)
            {
                *alternate = switch;
            }
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    /// Text, control characters, and anything that is not a whole escape
    /// sequence.
    Bytes(&'a [u8]),
    /// One whole escape sequence, from its ESC to its final byte.
    Sequence(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    String,
    StringEscape,
}

/// Splits a byte stream into escape sequences and everything else, across
/// reads, the way a VT500-style terminal parses it.
#[derive(Default)]
struct Scanner {
    state: State,
    pending: Vec<u8>,
    /// The current sequence grew past its limit and is being passed through
    /// byte by byte.
    passing_through: bool,
}

enum Step {
    Consumed,
    Reprocess,
}

impl Scanner {
    fn feed(&mut self, input: &[u8], sink: &mut dyn FnMut(Piece<'_>)) {
        let mut run_start = 0;
        let mut index = 0;
        while index < input.len() {
            let byte = input[index];
            if self.state == State::Ground {
                if byte == ESC {
                    if run_start < index {
                        sink(Piece::Bytes(&input[run_start..index]));
                    }
                    self.pending.clear();
                    self.pending.push(ESC);
                    self.state = State::Escape;
                }
                index += 1;
                continue;
            }
            match self.advance(byte, sink) {
                Step::Consumed => index += 1,
                Step::Reprocess => {}
            }
            if self.state == State::Ground {
                run_start = index;
            }
        }
        if self.state == State::Ground && run_start < input.len() {
            sink(Piece::Bytes(&input[run_start..]));
        }
    }

    /// Hands out an unfinished sequence as it is.
    fn finish(&mut self, sink: &mut dyn FnMut(Piece<'_>)) {
        self.abort(sink);
    }

    fn advance(&mut self, byte: u8, sink: &mut dyn FnMut(Piece<'_>)) -> Step {
        match self.state {
            State::Ground => Step::Reprocess,
            State::Escape | State::EscapeIntermediate => match byte {
                b'[' if self.state == State::Escape => self.push(byte, State::Csi, sink),
                b']' | b'P' | b'X' | b'^' | b'_' if self.state == State::Escape => {
                    self.push(byte, State::String, sink)
                }
                0x20..=0x2f => self.push(byte, State::EscapeIntermediate, sink),
                0x30..=0x7e => self.complete(byte, sink),
                _ => self.interrupt(byte, sink),
            },
            State::Csi => match byte {
                0x40..=0x7e => self.complete(byte, sink),
                ESC | CAN | SUB | 0x80.. => self.interrupt(byte, sink),
                // Parameters, intermediates, and C0 controls, which a terminal
                // carries out without ending the sequence.
                _ => self.push(byte, State::Csi, sink),
            },
            State::String => match byte {
                BEL => self.complete(byte, sink),
                ESC => {
                    // Kept back even when passing through: whether it ends
                    // the string or starts the next sequence is known only
                    // from the byte after it.
                    self.pending.push(byte);
                    self.state = State::StringEscape;
                    Step::Consumed
                }
                CAN | SUB => self.interrupt(byte, sink),
                _ => self.push(byte, State::String, sink),
            },
            State::StringEscape => {
                if byte == b'\\' {
                    return self.complete(byte, sink);
                }
                // An ESC that is not a string terminator ends the string and
                // starts a new sequence.
                self.pending.pop();
                if self.passing_through {
                    self.passing_through = false;
                } else {
                    sink(Piece::Sequence(&self.pending));
                }
                self.pending.clear();
                self.pending.push(ESC);
                self.state = State::Escape;
                Step::Reprocess
            }
        }
    }

    fn push(&mut self, byte: u8, next: State, sink: &mut dyn FnMut(Piece<'_>)) -> Step {
        self.state = next;
        if self.passing_through {
            if !self.pending.is_empty() {
                sink(Piece::Bytes(&self.pending));
                self.pending.clear();
            }
            sink(Piece::Bytes(&[byte]));
            return Step::Consumed;
        }
        self.pending.push(byte);
        let limit = match next {
            State::String | State::StringEscape => MAX_STRING_SEQUENCE_LEN,
            _ => MAX_CONTROL_SEQUENCE_LEN,
        };
        if self.pending.len() > limit {
            sink(Piece::Bytes(&self.pending));
            self.pending.clear();
            self.passing_through = true;
        }
        Step::Consumed
    }

    fn complete(&mut self, byte: u8, sink: &mut dyn FnMut(Piece<'_>)) -> Step {
        self.pending.push(byte);
        if self.passing_through {
            sink(Piece::Bytes(&self.pending));
        } else {
            sink(Piece::Sequence(&self.pending));
        }
        self.reset();
        Step::Consumed
    }

    /// A byte that ends the sequence without completing it: CAN and SUB
    /// cancel it, ESC starts another one, and anything else is handled as if
    /// no sequence had been started.
    fn interrupt(&mut self, byte: u8, sink: &mut dyn FnMut(Piece<'_>)) -> Step {
        match byte {
            CAN | SUB => {
                self.pending.push(byte);
                self.abort(sink);
                Step::Consumed
            }
            ESC => {
                self.abort(sink);
                self.pending.push(ESC);
                self.state = State::Escape;
                Step::Consumed
            }
            _ => {
                self.abort(sink);
                Step::Reprocess
            }
        }
    }

    fn abort(&mut self, sink: &mut dyn FnMut(Piece<'_>)) {
        if !self.pending.is_empty() {
            sink(Piece::Bytes(&self.pending));
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.passing_through = false;
        self.state = State::Ground;
    }
}

struct ControlSequence {
    prefix: Option<u8>,
    parameters: Vec<u8>,
    intermediates: Vec<u8>,
    final_byte: u8,
}

impl ControlSequence {
    /// Parses what follows `ESC [` (or `ESC P`, whose header has the same
    /// shape) up to and including the final byte.
    fn parse(body: &[u8]) -> Option<Self> {
        let (final_index, &final_byte) = body
            .iter()
            .enumerate()
            .find(|(_, byte)| (0x40..=0x7e).contains(*byte))?;
        let header = &body[..final_index];
        let (prefix, rest) = match header.first() {
            Some(&prefix @ b'<'..=b'?') => (Some(prefix), &header[1..]),
            _ => (None, header),
        };
        let mut parameters = Vec::new();
        let mut intermediates = Vec::new();
        for &byte in rest {
            match byte {
                0x30..=0x3f => parameters.push(byte),
                0x20..=0x2f => intermediates.push(byte),
                _ => {}
            }
        }
        Some(Self {
            prefix,
            parameters,
            intermediates,
            final_byte,
        })
    }

    fn parameters(&self) -> impl Iterator<Item = Option<u32>> + '_ {
        self.parameters
            .split(|&byte| byte == b';')
            .map(|parameter| {
                let digits = parameter.split(|&byte| byte == b':').next().unwrap_or(&[]);
                std::str::from_utf8(digits).ok()?.parse().ok()
            })
    }

    fn first_parameter(&self) -> Option<u32> {
        self.parameters().next().flatten()
    }
}

fn is_terminal_query(sequence: &[u8]) -> bool {
    match sequence.get(1) {
        Some(b'[') => ControlSequence::parse(&sequence[2..]).is_some_and(|csi| is_csi_query(&csi)),
        Some(b']') => is_osc_query(string_payload(&sequence[2..])),
        Some(b'P') => ControlSequence::parse(&sequence[2..]).is_some_and(|dcs| {
            dcs.final_byte == b'q' && matches!(dcs.intermediates.as_slice(), b"$" | b"+")
        }),
        // DECID, the old way of asking for the device attributes.
        Some(b'Z') => sequence.len() == 2,
        _ => false,
    }
}

fn is_csi_query(csi: &ControlSequence) -> bool {
    let intermediates = csi.intermediates.as_slice();
    match (csi.prefix, intermediates, csi.final_byte) {
        // Device attributes (primary, secondary, tertiary).
        (_, _, b'c') => true,
        // Device status reports, cursor position reports included.
        (_, _, b'n') => true,
        // DECRQM, DECRQTSR, DECRQPSR, and DECRQCRA.
        (_, b"$", b'p' | b'u' | b'w') | (_, b"*", b'y') => true,
        // XTVERSION.
        (Some(b'>'), b"", b'q') => true,
        // The reporting window operations.
        (None, b"", b't') => csi
            .first_parameter()
            .is_some_and(|operation| (11..=21).contains(&operation)),
        // The kitty keyboard protocol's query, XTQMODKEYS, and XTSMGRAPHICS.
        (Some(b'?'), b"", b'u' | b'm' | b'S') => true,
        // DECREQTPARM.
        (None, b"", b'x') => true,
        _ => false,
    }
}

fn string_payload(body: &[u8]) -> &[u8] {
    body.strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(&[BEL]))
        .unwrap_or(body)
}

/// OSC color, palette and clipboard queries: a `?` in place of a value.
fn is_osc_query(payload: &[u8]) -> bool {
    payload
        .split(|&byte| byte == b';')
        .skip(1)
        .any(|field| field == b"?")
}

/// `Some(true)` for a sequence that switches to the alternate screen,
/// `Some(false)` for one that switches back.
fn alternate_screen_switch(sequence: &[u8]) -> Option<bool> {
    match sequence.get(1)? {
        b'[' => {
            let csi = ControlSequence::parse(&sequence[2..])?;
            let enters = match (csi.prefix, csi.intermediates.as_slice(), csi.final_byte) {
                (Some(b'?'), b"", b'h') => true,
                (Some(b'?'), b"", b'l') => false,
                _ => return None,
            };
            csi.parameters()
                .any(|mode| matches!(mode, Some(47 | 1047 | 1049)))
                .then_some(enters)
        }
        // RIS resets the terminal, back to the main screen.
        b'c' if sequence.len() == 2 => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(chunks: &[&[u8]]) -> Vec<(bool, Vec<u8>)> {
        let mut pieces = Vec::new();
        let mut scanner = Scanner::default();
        let mut sink = |piece: Piece<'_>| match piece {
            Piece::Bytes(bytes) => pieces.push((false, bytes.to_vec())),
            Piece::Sequence(sequence) => pieces.push((true, sequence.to_vec())),
        };
        for chunk in chunks {
            scanner.feed(chunk, &mut sink);
        }
        scanner.finish(&mut sink);
        // Merge neighbouring text so that the split of the input does not
        // show in the result.
        let mut merged: Vec<(bool, Vec<u8>)> = Vec::new();
        for (is_sequence, bytes) in pieces {
            match merged.last_mut() {
                Some((false, last)) if !is_sequence => last.extend(bytes),
                _ => merged.push((is_sequence, bytes)),
            }
        }
        merged
    }

    #[test]
    fn sequences_are_found_across_reads() {
        let input: &[u8] = b"a\x1b[1;31mred\x1b]0;title\x07\x1bP$qm\x1b\\\x1b7b";
        let expected = vec![
            (false, b"a".to_vec()),
            (true, b"\x1b[1;31m".to_vec()),
            (false, b"red".to_vec()),
            (true, b"\x1b]0;title\x07".to_vec()),
            (true, b"\x1bP$qm\x1b\\".to_vec()),
            (true, b"\x1b7".to_vec()),
            (false, b"b".to_vec()),
        ];
        assert_eq!(pieces(&[input]), expected);
        for split in 1..input.len() {
            let (first, second) = input.split_at(split);
            assert_eq!(pieces(&[first, second]), expected, "split at {split}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(pieces(&bytes), expected);
    }

    #[test]
    fn interrupted_sequences_are_passed_on() {
        // CAN cancels, ESC restarts, a non-ASCII byte ends it.
        assert_eq!(
            pieces(&[b"\x1b[6\x18x"]),
            vec![(false, b"\x1b[6\x18x".to_vec())]
        );
        assert_eq!(
            pieces(&[b"\x1b[6\x1b[6n"]),
            vec![(false, b"\x1b[6".to_vec()), (true, b"\x1b[6n".to_vec())]
        );
        assert_eq!(
            pieces(&["\x1b[あ".as_bytes()]),
            vec![(false, "\x1b[あ".as_bytes().to_vec())]
        );
        // An OSC ended by an ESC that starts another sequence.
        assert_eq!(
            pieces(&[b"\x1b]11;?\x1b[0m"]),
            vec![(true, b"\x1b]11;?".to_vec()), (true, b"\x1b[0m".to_vec())]
        );
        // What is left at the end is kept as it is.
        assert_eq!(pieces(&[b"x\x1b[6"]), vec![(false, b"x\x1b[6".to_vec())]);
    }

    #[test]
    fn overlong_sequences_pass_through() {
        let mut input = b"\x1b]52;c;".to_vec();
        input.extend(std::iter::repeat_n(b'A', MAX_STRING_SEQUENCE_LEN + 10));
        input.extend(b"\x07after");
        let merged = pieces(&[&input]);
        assert_eq!(merged, vec![(false, input.clone())]);
        assert_eq!(sanitize(&input), input);

        let mut input = b"\x1b]0;".to_vec();
        input.extend(std::iter::repeat_n(b'A', MAX_STRING_SEQUENCE_LEN + 10));
        input.extend(b"\x1b[6n");
        let sanitized = sanitize(&input);
        assert_eq!(sanitized, input[..input.len() - 4].to_vec());
    }

    #[test]
    fn queries_are_removed_from_the_replay() {
        let queries: &[&[u8]] = &[
            b"\x1b[c",
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[>0c",
            b"\x1b[=c",
            b"\x1b[5n",
            b"\x1b[6n",
            b"\x1b[?6n",
            b"\x1b[?15n",
            b"\x1b[?996n",
            b"\x1b[?2026$p",
            b"\x1b[4$p",
            b"\x1b[>q",
            b"\x1b[>0q",
            b"\x1b[11t",
            b"\x1b[14t",
            b"\x1b[14;2t",
            b"\x1b[18t",
            b"\x1b[19t",
            b"\x1b[21t",
            b"\x1b[?u",
            b"\x1b[?4m",
            b"\x1b[1$w",
            b"\x1b[1;1;1;1;1;1*y",
            b"\x1b[?1;1;0S",
            b"\x1b[x",
            b"\x1b]4;1;?\x07",
            b"\x1b]4;1;?;2;?\x1b\\",
            b"\x1b]10;?\x07",
            b"\x1b]11;?\x1b\\",
            b"\x1b]12;?\x07",
            b"\x1b]17;?\x07",
            b"\x1b]52;c;?\x07",
            b"\x1bP$qm\x1b\\",
            b"\x1bP$q\"q\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1bZ",
            b"\x05",
        ];
        for query in queries {
            let mut input = b"before".to_vec();
            input.extend_from_slice(query);
            input.extend_from_slice(b"after");
            assert_eq!(
                sanitize(&input),
                b"beforeafter".to_vec(),
                "{:?} is removed",
                String::from_utf8_lossy(query)
            );
        }
    }

    #[test]
    fn everything_else_is_replayed_as_it_was() {
        let kept: &[&[u8]] = &[
            b"\x1b[1;31m",
            b"\x1b[0m",
            b"\x1b[2J",
            b"\x1b[H",
            b"\x1b[10;20H",
            b"\x1b[2 q",
            b"\x1b[\"q",
            b"\x1b[u",
            b"\x1b[s",
            b"\x1b[>1u",
            b"\x1b[<u",
            b"\x1b[=1;1u",
            b"\x1b[>4;1m",
            b"\x1b[>4n\x1b[22t",
            b"\x1b[8;24;80t",
            b"\x1b[?1049h",
            b"\x1b[?25l",
            b"\x1b[5S",
            b"\x1b[3*x",
            b"\x1b]0;title\x07",
            b"\x1b]7;file://host/tmp\x1b\\",
            b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07",
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]11;rgb:0000/0000/0000\x07",
            b"\x1b]104\x07",
            b"\x1bP0;1;0q#0;2;0;0;0~\x1b\\",
            b"\x1b_Gf=100;AAAA\x1b\\",
            b"\x1b7\x1b8",
            b"\x1b(B",
            b"\x1bc",
            "日本語\r\n".as_bytes(),
        ];
        for sequence in kept {
            let expected_removal = sequence.starts_with(b"\x1b[>4n");
            let sanitized = sanitize(sequence);
            if expected_removal {
                assert_eq!(sanitized, b"\x1b[22t".to_vec());
            } else {
                assert_eq!(
                    sanitized,
                    sequence.to_vec(),
                    "{:?} is kept",
                    String::from_utf8_lossy(sequence)
                );
            }
        }
    }

    #[test]
    fn the_history_keeps_its_tail() {
        let mut history = OutputHistory::new(10);
        history.push(b"12345");
        history.push(b"6789");
        let replay = history.replay();
        assert_eq!(replay.bytes, b"123456789".to_vec());
        assert!(!replay.needs_redraw);

        history.push(b"ab");
        assert_eq!(
            history.buffer.iter().copied().collect::<Vec<_>>(),
            b"23456789ab".to_vec()
        );
        assert!(history.dropped);

        let mut history = OutputHistory::new(4);
        history.push(b"abcdefgh");
        assert_eq!(
            history.buffer.iter().copied().collect::<Vec<_>>(),
            b"efgh".to_vec()
        );
        assert!(history.dropped);

        let mut history = OutputHistory::new(4);
        history.push(b"abcd");
        assert!(!history.dropped, "exactly full is not truncated");
    }

    #[test]
    fn a_truncated_replay_starts_at_a_line() {
        let mut history = OutputHistory::new(16);
        history.push(b"line one\r\nline two\r\nline three\r\n");
        let replay = history.replay();
        let mut expected = TRUNCATED_REPLAY_NOTICE.as_bytes().to_vec();
        expected.extend(b"line three\r\n");
        assert_eq!(replay.bytes, expected);

        // The cut never lands inside an escape sequence.
        let mut history = OutputHistory::new(20);
        history.push(b"\x1b[38;5;196mred text\r\n\x1b[1mbold\x1b[0m\r\n");
        let replay = history.replay();
        let mut expected = TRUNCATED_REPLAY_NOTICE.as_bytes().to_vec();
        expected.extend(b"\x1b[1mbold\x1b[0m\r\n");
        assert_eq!(replay.bytes, expected);
    }

    #[test]
    fn a_long_line_followed_by_more_keeps_what_came_after_it() {
        let mut history = OutputHistory::new(4 * 1024 * 1024);
        let flood = vec![b'x'; 8_000_000];
        // Reads of a pseudo-terminal end wherever its buffers happen to.
        let mut sizes = [4095, 65536, 1, 30_000, 4096].into_iter().cycle();
        let mut rest = flood.as_slice();
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at(rest.len().min(sizes.next().unwrap_or(1)));
            history.push(chunk);
            rest = tail;
        }
        history.push(b"\r\nafter the flood\r\n$ ");
        let replay = history.replay();
        let mut expected = TRUNCATED_REPLAY_NOTICE.as_bytes().to_vec();
        expected.extend(b"after the flood\r\n$ ");
        assert_eq!(
            String::from_utf8_lossy(&replay.bytes),
            String::from_utf8_lossy(&expected)
        );
    }

    #[test]
    fn a_truncated_replay_without_line_breaks() {
        assert_eq!(replay_start(b"abc\rdef"), 4);
        assert_eq!(replay_start(b"abc\ndef\rghi"), 4);
        assert_eq!(replay_start(b"abcdef"), 6);
        assert_eq!(replay_start(b""), 0);

        let mut history = OutputHistory::new(4);
        history.push(b"abcdefgh");
        assert_eq!(
            history.replay().bytes,
            TRUNCATED_REPLAY_NOTICE.as_bytes().to_vec()
        );
    }

    #[test]
    fn a_program_on_the_alternate_screen_is_redrawn() {
        let mut history = OutputHistory::new(1024);
        history.push(b"$ vim\r\n\x1b[?10");
        history.push(b"49h\x1b[2Jcontent");
        let replay = history.replay();
        assert!(replay.needs_redraw);
        assert_eq!(replay.bytes, b"$ vim\r\n\x1b[?1049h\x1b[2Jcontent".to_vec());

        history.push(b"\x1b[?1049l$ ");
        assert!(!history.replay().needs_redraw);

        for (enter, leave) in [
            (&b"\x1b[?1047h"[..], &b"\x1b[?1047l"[..]),
            (b"\x1b[?47h", b"\x1b[?47l"),
            (b"\x1b[?1;1049h", b"\x1bc"),
        ] {
            let mut history = OutputHistory::new(1024);
            history.push(enter);
            assert!(history.replay().needs_redraw);
            history.push(leave);
            assert!(!history.replay().needs_redraw);
        }

        let mut history = OutputHistory::new(1024);
        history.push(b"\x1b[?1h\x1b[?25l");
        assert!(!history.replay().needs_redraw);
    }

    #[test]
    fn the_alternate_screen_is_entered_again_when_its_start_was_dropped() {
        let mut history = OutputHistory::new(16);
        history.push(b"\x1b[?1049h");
        history.push(b"row 1\r\nrow 2\r\nrow 3\r\n");
        let replay = history.replay();
        assert!(replay.needs_redraw);
        assert!(
            replay.bytes.ends_with(ENTER_ALTERNATE_SCREEN),
            "{:?}",
            String::from_utf8_lossy(&replay.bytes)
        );
    }
}
