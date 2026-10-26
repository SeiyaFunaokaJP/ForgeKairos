//! Working directory tracking for remote shells, via OSC 7.
//!
//! A local shell's directory comes from [`crate::pty_info`], which polls the
//! child process. A remote shell runs on the other side of an `ssh` process
//! whose own directory never changes, so the only thing the client can observe
//! is what the shell writes to the PTY. OSC 7
//! (`ESC ] 7 ; file://host/path BEL`) is the de-facto way shells report it:
//! fish and oh-my-zsh emit it unprompted, and bash is asked to by
//! [`osc7_prompt_command_env`]. Neither alacritty nor its `vte` parser
//! understand the sequence, so it is sniffed off the raw PTY byte stream
//! (see `crate::alacritty::TeePty`) before the emulator discards it.
//!
//! Tracking it lets a remote terminal whose connection dropped restart in the
//! directory the user was actually in instead of at the worktree root.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use util::{paths::PathStyle, shell::ShellKind};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// OSC 7 payloads above this size are dropped rather than buffered. A directory
/// path never comes close, and a shell that emits a runaway sequence must not
/// be able to grow this buffer without bound.
const MAX_PAYLOAD_LEN: usize = 4096;

/// Shared handle to the last directory a remote shell reported. Cloning is
/// cheap and every clone sees the same directory, so the PTY reader thread and
/// the terminal can hold one each.
#[derive(Clone)]
pub(crate) struct RemoteCwdTracker {
    path_style: PathStyle,
    state: Arc<Mutex<TrackerState>>,
}

#[derive(Default)]
struct TrackerState {
    scanner: Osc7Scanner,
    /// The host named by the first report that named one. A later report from a
    /// different host comes from a shell the user opened on a *third* machine
    /// (an `ssh` typed inside the terminal), whose directories do not exist on
    /// the host we reconnect to.
    host: Option<String>,
    current: Option<PathBuf>,
}

impl RemoteCwdTracker {
    pub(crate) fn new(path_style: PathStyle) -> Self {
        Self {
            path_style,
            state: Arc::new(Mutex::new(TrackerState::default())),
        }
    }

    /// Called from the PTY reader thread for every chunk read.
    pub(crate) fn record(&self, bytes: &[u8]) {
        let mut state = self.state.lock();
        let mut rest = bytes;
        while let Some((payload, tail)) = state.scanner.scan(rest) {
            rest = tail;
            let Some((host, working_directory)) = parse_file_url(&payload, self.path_style) else {
                continue;
            };
            if !host.is_empty() {
                match &state.host {
                    Some(known_host) if *known_host != host => continue,
                    Some(_) => {}
                    None => state.host = Some(host),
                }
            }
            state.current = Some(working_directory);
        }
    }

    /// Records the directory the shell was asked to start in. A shell that
    /// never reports one — because it is not configured to, or because the
    /// connection dropped again before its first prompt — then still restarts
    /// where it began instead of at the worktree root. A report that already
    /// arrived is never overwritten.
    pub(crate) fn record_start_directory(&self, working_directory: PathBuf) {
        let mut state = self.state.lock();
        state.current.get_or_insert(working_directory);
    }

    pub(crate) fn current(&self) -> Option<PathBuf> {
        self.state.lock().current.clone()
    }
}

/// Written to the PTY by [`shell_command_with_cwd_fallback`] when the directory
/// it was asked to start in is gone. Bracketed like
/// [`crate::REMOTE_CONNECTION_LOST_NOTICE`] so it reads as Zed's own line rather
/// than as output of the shell.
fn missing_directory_notice(working_directory: &str) -> String {
    i18n::t!("[{working_directory} was not found, so the shell started in the home directory]")
}

/// The command that starts a remote login shell in `working_directory`, or in
/// the user's home — with a line saying so — when that directory no longer
/// exists on the host. `None` when the directory or the shell cannot be quoted.
///
/// Without it the directory is entered by the command line `ssh` hands to the
/// login shell (`cd <dir> && exec …`), so a directory that was renamed or
/// deleted since it was recorded takes the whole shell down with it: the tab
/// dies with a status that the reconnect handling does not recognize, and the
/// user loses the tab as well as the directory.
///
/// The wrapper is `sh` rather than the login shell because the fallback has to
/// be written in one dialect: fish emits OSC 7 unprompted, and so is the shell
/// most likely to be restarted into a tracked directory, but it has no
/// `{ …; }` command group.
pub fn shell_command_with_cwd_fallback(
    working_directory: &Path,
    remote_shell: &str,
) -> Option<(String, Vec<String>)> {
    let shell_kind = ShellKind::Posix;
    let working_directory = working_directory.to_str()?;
    let quoted_directory = shell_kind.try_quote(working_directory)?;
    let quoted_shell = shell_kind.try_quote(remote_shell)?;
    let notice = missing_directory_notice(working_directory);
    let quoted_notice = shell_kind.try_quote(&notice)?;
    Some((
        "sh".to_string(),
        vec![
            "-c".to_string(),
            format!(
                "cd {quoted_directory} 2>/dev/null || {{ printf '%s\\r\\n' {quoted_notice}; cd; }}; \
                 exec {quoted_shell} -l"
            ),
        ],
    ))
}

/// Percent-encodes `%` before ` ` so that the escapes the first substitution
/// introduces are not encoded a second time. Other characters are left alone:
/// `printf` is the only encoder every bash is guaranteed to have, and
/// [`parse_file_url`] accepts a path that was never encoded.
const OSC7_PROMPT_COMMAND: &str = concat!(
    "__zed_osc7_cwd=${PWD//%/%25}; ",
    r#"printf '\033]7;file://%s%s\a' "${HOSTNAME:-}" "${__zed_osc7_cwd// /%20}""#,
);

/// The environment bash needs in order to report its directory with OSC 7, or
/// `None` for shells that cannot be asked through the environment.
///
/// `PROMPT_COMMAND` is the only such hook bash has. A user rc file that assigns
/// to `PROMPT_COMMAND` (rather than appending) overrides it, in which case
/// tracking simply stays off — rewriting the user's rc files to prevent that is
/// not worth the intrusion. zsh and fish have no equivalent environment hook,
/// so they are tracked only when their own configuration emits OSC 7; fish and
/// oh-my-zsh both do.
pub fn osc7_prompt_command_env(remote_shell: &str) -> Option<(String, String)> {
    // `sh` is bash on several distributions, and on the rest it is a shell that
    // ignores `PROMPT_COMMAND` (dash, ksh), where the variable is inert.
    let shell_name = remote_shell.rsplit(['/', '\\']).next()?;
    if !matches!(shell_name, "bash" | "sh") {
        return None;
    }
    Some((
        "PROMPT_COMMAND".to_string(),
        OSC7_PROMPT_COMMAND.to_string(),
    ))
}

/// Splits `file://host/path` into its host (empty when the URL names none) and
/// its path. Returns `None` for anything that is not a usable absolute path.
fn parse_file_url(payload: &[u8], path_style: PathStyle) -> Option<(String, PathBuf)> {
    let url = std::str::from_utf8(payload).ok()?;
    let rest = url
        .strip_prefix("file://")
        .or_else(|| url.strip_prefix("file:"))?;
    let (host, encoded_path) = match rest.find('/') {
        Some(0) => ("", rest),
        Some(separator) => rest.split_at(separator),
        None => return None,
    };
    let decoded = percent_encoding::percent_decode_str(encoded_path)
        .decode_utf8()
        .ok()?;

    // A drive-letter path arrives as `/C:/Users/…`, where the leading separator
    // belongs to the URL rather than to the path.
    let path = match path_style {
        PathStyle::Windows => decoded.strip_prefix('/').unwrap_or(&decoded),
        PathStyle::Unix => &decoded,
    };
    if !path_style.has_root(Path::new(path)) || path.chars().any(|character| character.is_control())
    {
        return None;
    }
    Some((host.to_string(), PathBuf::from(path)))
}

#[derive(Clone, Copy, Default, PartialEq)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    /// Inside an OSC string that still looks like an OSC 7.
    Osc,
    /// Inside an OSC string that is not an OSC 7, or that outgrew
    /// [`MAX_PAYLOAD_LEN`]; its bytes are discarded until the terminator.
    OscDiscarded,
    /// Saw `ESC` inside an OSC string, where `\` completes the ST terminator.
    OscEscape {
        discarded: bool,
    },
}

/// Finds OSC 7 sequences in a byte stream that arrives in arbitrary chunks.
#[derive(Default)]
struct Osc7Scanner {
    state: ScanState,
    payload: Vec<u8>,
}

impl Osc7Scanner {
    /// Consumes `bytes` up to and including the end of the next complete OSC 7,
    /// returning its payload (everything after `7;`) and the unconsumed rest.
    /// `None` means the whole slice was consumed without completing one.
    fn scan<'a>(&mut self, bytes: &'a [u8]) -> Option<(Vec<u8>, &'a [u8])> {
        let mut rest = bytes;
        loop {
            if self.state == ScanState::Ground {
                let escape = rest.iter().position(|byte| *byte == ESC)?;
                self.state = ScanState::Escape;
                rest = rest.get(escape + 1..)?;
                continue;
            }
            let (byte, tail) = rest.split_first()?;
            rest = tail;
            if let Some(payload) = self.feed(*byte) {
                return Some((payload, rest));
            }
        }
    }

    /// Returns the payload of the OSC 7 this byte completed, if it completed one.
    fn feed(&mut self, byte: u8) -> Option<Vec<u8>> {
        match self.state {
            ScanState::Ground => {
                if byte == ESC {
                    self.state = ScanState::Escape;
                }
            }
            ScanState::Escape => {
                self.state = match byte {
                    b']' => {
                        self.payload.clear();
                        ScanState::Osc
                    }
                    ESC => ScanState::Escape,
                    _ => ScanState::Ground,
                }
            }
            ScanState::Osc => match byte {
                BEL => {
                    self.state = ScanState::Ground;
                    return self.take_payload();
                }
                ESC => self.state = ScanState::OscEscape { discarded: false },
                // Any other C0 control ends the string without dispatching it:
                // an OSC that runs into one was never well formed.
                0x00..=0x1f => self.state = ScanState::Ground,
                _ => {
                    self.payload.push(byte);
                    let no_longer_osc7 = match self.payload.len() {
                        1 => byte != b'7',
                        2 => byte != b';',
                        _ => false,
                    };
                    if no_longer_osc7 || self.payload.len() > MAX_PAYLOAD_LEN {
                        self.payload.clear();
                        self.state = ScanState::OscDiscarded;
                    }
                }
            },
            ScanState::OscDiscarded => match byte {
                BEL => self.state = ScanState::Ground,
                ESC => self.state = ScanState::OscEscape { discarded: true },
                0x00..=0x1f => self.state = ScanState::Ground,
                _ => {}
            },
            ScanState::OscEscape { discarded } => {
                let terminated = byte == b'\\';
                self.state = if byte == ESC {
                    ScanState::Escape
                } else {
                    ScanState::Ground
                };
                if terminated && !discarded {
                    return self.take_payload();
                }
                self.payload.clear();
            }
        }
        None
    }

    /// Splits the `7;` introducer off the buffered payload, leaving the buffer
    /// empty for the next sequence.
    fn take_payload(&mut self) -> Option<Vec<u8>> {
        if self.payload.len() < 2 {
            self.payload.clear();
            return None;
        }
        let mut payload = std::mem::take(&mut self.payload);
        Some(payload.split_off(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let mut scanner = Osc7Scanner::default();
        let mut payloads = Vec::new();
        for chunk in chunks {
            let mut rest = *chunk;
            while let Some((payload, tail)) = scanner.scan(rest) {
                payloads.push(String::from_utf8_lossy(&payload).into_owned());
                rest = tail;
            }
        }
        payloads
    }

    #[test]
    fn test_scan_finds_bel_and_st_terminated_sequences() {
        assert_eq!(
            collect(&[b"\x1b]7;file:///a\x07"]),
            vec!["file:///a".to_string()]
        );
        assert_eq!(
            collect(&[b"\x1b]7;file:///a\x1b\\"]),
            vec!["file:///a".to_string()]
        );
    }

    #[test]
    fn test_scan_spans_chunk_boundaries() {
        assert_eq!(
            collect(&[b"ls\r\n\x1b]7;fi", b"le://host/tmp", b"\x07$ "]),
            vec!["file://host/tmp".to_string()]
        );
    }

    #[test]
    fn test_scan_finds_multiple_sequences_in_one_chunk() {
        assert_eq!(
            collect(&[b"\x1b]7;file:///a\x07x\x1b]0;title\x07y\x1b]7;file:///b\x07"]),
            vec!["file:///a".to_string(), "file:///b".to_string()]
        );
    }

    #[test]
    fn test_scan_ignores_other_osc_and_unterminated_payloads() {
        assert!(collect(&[b"\x1b]52;c;QUJD\x07"]).is_empty());
        assert!(collect(&[b"\x1b]7;file:///a"]).is_empty());
        assert!(collect(&[b"\x1b]7;file:///a\r\n"]).is_empty());
    }

    #[test]
    fn test_scan_drops_oversized_payloads() {
        let mut sequence = b"\x1b]7;".to_vec();
        sequence.resize(sequence.len() + MAX_PAYLOAD_LEN + 1, b'a');
        sequence.push(BEL);
        sequence.extend_from_slice(b"\x1b]7;file:///a\x07");

        assert_eq!(collect(&[&sequence]), vec!["file:///a".to_string()]);
    }

    #[test]
    fn test_parse_file_url_reads_host_and_path() {
        assert_eq!(
            parse_file_url(b"file://example/home/user", PathStyle::Unix),
            Some(("example".to_string(), PathBuf::from("/home/user")))
        );
        assert_eq!(
            parse_file_url(b"file:///home/user", PathStyle::Unix),
            Some((String::new(), PathBuf::from("/home/user")))
        );
        assert_eq!(
            parse_file_url(b"file:/home/user", PathStyle::Unix),
            Some((String::new(), PathBuf::from("/home/user")))
        );
    }

    #[test]
    fn test_parse_file_url_decodes_percent_escapes() {
        assert_eq!(
            parse_file_url(b"file://h/home/my%20work/100%25", PathStyle::Unix),
            Some(("h".to_string(), PathBuf::from("/home/my work/100%")))
        );
    }

    #[test]
    fn test_parse_file_url_rejects_unusable_paths() {
        assert_eq!(parse_file_url(b"file://host", PathStyle::Unix), None);
        assert_eq!(parse_file_url(b"http://host/tmp", PathStyle::Unix), None);
        assert_eq!(parse_file_url(b"file://host/rel", PathStyle::Windows), None);
        assert_eq!(
            parse_file_url(b"file://host/tmp/%00bad", PathStyle::Unix),
            None
        );
    }

    #[test]
    fn test_parse_file_url_strips_the_url_slash_from_drive_letters() {
        assert_eq!(
            parse_file_url(b"file://host/C:/Users/user", PathStyle::Windows),
            Some(("host".to_string(), PathBuf::from("C:/Users/user")))
        );
    }

    #[test]
    fn test_tracker_keeps_the_last_reported_directory() {
        let tracker = RemoteCwdTracker::new(PathStyle::Unix);
        assert_eq!(tracker.current(), None);

        tracker.record(b"\x1b]7;file://host/home/user\x07");
        assert_eq!(tracker.current(), Some(PathBuf::from("/home/user")));

        tracker.record(b"\x1b]7;file://host/srv\x07");
        assert_eq!(tracker.current(), Some(PathBuf::from("/srv")));
    }

    #[test]
    fn test_tracker_ignores_reports_from_another_host() {
        let tracker = RemoteCwdTracker::new(PathStyle::Unix);
        tracker.record(b"\x1b]7;file://host/home/user\x07");
        tracker.record(b"\x1b]7;file://nested-host/root\x07");

        assert_eq!(tracker.current(), Some(PathBuf::from("/home/user")));

        // A shell that names no host is still the one we started.
        tracker.record(b"\x1b]7;file:///srv\x07");
        assert_eq!(tracker.current(), Some(PathBuf::from("/srv")));
    }

    #[test]
    fn test_tracker_start_directory_is_a_fallback_not_an_override() {
        let tracker = RemoteCwdTracker::new(PathStyle::Unix);
        tracker.record_start_directory(PathBuf::from("/srv/app"));
        assert_eq!(tracker.current(), Some(PathBuf::from("/srv/app")));

        tracker.record(b"\x1b]7;file://host/home/user\x07");
        assert_eq!(tracker.current(), Some(PathBuf::from("/home/user")));

        // A restart seeds the tracker of the *new* terminal, so a report that
        // already arrived must win over a later seeding attempt as well.
        tracker.record_start_directory(PathBuf::from("/srv/app"));
        assert_eq!(tracker.current(), Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn test_shell_command_with_cwd_fallback_quotes_and_falls_back_to_home() {
        let (program, args) =
            shell_command_with_cwd_fallback(Path::new("/srv/my app"), "/usr/bin/fish")
                .expect("a quotable directory and shell");
        assert_eq!(program, "sh");
        let [dash_c, script] = args.as_slice() else {
            panic!("expected `sh -c <script>`, got {args:?}");
        };
        assert_eq!(dash_c, "-c");
        assert_eq!(
            script,
            "cd '/srv/my app' 2>/dev/null || { printf '%s\\r\\n' \
             '[/srv/my app was not found, so the shell started in the home directory]'; cd; }; \
             exec /usr/bin/fish -l"
        );
    }

    #[test]
    fn test_shell_command_with_cwd_fallback_rejects_non_utf8_directories() {
        #[cfg(unix)]
        {
            use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};

            assert_eq!(
                shell_command_with_cwd_fallback(
                    Path::new(OsStr::from_bytes(b"/srv/\xff")),
                    "/bin/bash"
                ),
                None
            );
        }
        assert_eq!(
            shell_command_with_cwd_fallback(Path::new("/srv/\0bad"), "/bin/bash"),
            None
        );
    }

    #[test]
    fn test_osc7_prompt_command_env_is_offered_to_bash_only() {
        assert_eq!(
            osc7_prompt_command_env("/bin/bash").map(|(name, _)| name),
            Some("PROMPT_COMMAND".to_string())
        );
        assert_eq!(
            osc7_prompt_command_env("/bin/sh").map(|(name, _)| name),
            Some("PROMPT_COMMAND".to_string())
        );
        assert_eq!(osc7_prompt_command_env("/usr/bin/zsh"), None);
        assert_eq!(osc7_prompt_command_env("/usr/bin/fish"), None);
        assert_eq!(osc7_prompt_command_env("powershell.exe"), None);
    }
}
