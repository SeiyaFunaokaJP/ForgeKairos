//! Persistent remote terminals: `terminal-session attach` keeps the shell of a
//! remote terminal tab in a detached "holder" process, so that a dropped SSH
//! connection no longer kills the shell or loses its output. The holder
//! remembers the tail of the output and replays it on every attach.

#[cfg(unix)]
mod posix;
#[cfg(any(unix, test))]
mod replay;
#[cfg(any(unix, test))]
mod wire;

use anyhow::{Result, bail};
use clap::Subcommand;
use std::ffi::OsString;
#[cfg(any(unix, test))]
use std::path::Path;
#[cfg(any(unix, test))]
use std::time::Duration;

pub const DEFAULT_DETACHED_TIMEOUT_SECS: u64 = 86_400;
pub const DEFAULT_REPLAY_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_REPLAY_BYTES: u64 = 64 * 1024 * 1024;

const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;
#[cfg(any(unix, test))]
const EXIT_TAKEN_OVER: i32 = 253;
/// The attach lost the terminal or the holder while the shell lives on:
/// what `ssh` reports for a dropped connection, which is what the terminal
/// resumes the session after. Any other code would read as the shell's own
/// and close the tab, which ends the session.
#[cfg(any(unix, test))]
const EXIT_DETACHED: i32 = 255;

#[cfg(any(unix, test))]
fn resumed_without_shell_notice() -> String {
    format!(
        "\x1b[0m\r\n[{}]\r\n\x1b[0m",
        i18n::t!("The previous shell was no longer running. Starting a new shell")
    )
}
#[cfg(any(unix, test))]
fn taken_over_notice() -> String {
    format!(
        "\r\n[{}]\r\n",
        i18n::t!("Another connection took over this shell")
    )
}
#[cfg(any(unix, test))]
fn truncated_replay_notice() -> String {
    format!("\x1b[0m[… {} …]\r\n", i18n::t!("Older output was omitted"))
}
/// What the holder tells the attach that is there when the session is ended
/// on purpose (`terminal-session kill`, or a signal to the holder).
#[cfg(any(unix, test))]
const SESSION_ENDED_REASON: &str = "the terminal session was ended";

#[derive(Subcommand, Debug)]
pub enum TerminalSessionCommand {
    /// Connects this terminal to the shell kept for `--id`, starting a new
    /// one (with `-- <program> [args...]`) when there is none.
    Attach {
        #[arg(long)]
        id: String,
        #[arg(long)]
        nonce: String,
        #[arg(long)]
        resume: bool,
        #[arg(long, default_value_t = DEFAULT_DETACHED_TIMEOUT_SECS)]
        detached_timeout_secs: u64,
        #[arg(long, default_value_t = DEFAULT_REPLAY_BYTES)]
        replay_bytes: u64,
        #[arg(last = true, required = true)]
        program: Vec<OsString>,
    },
    /// Ends the shell kept for `--id`. Succeeds when there is none.
    Kill {
        #[arg(long)]
        id: String,
    },
    /// The detached process that keeps the shell. Started by `attach`.
    #[command(hide = true)]
    Holder {
        #[arg(long)]
        id: String,
        #[arg(long)]
        detached_timeout_secs: u64,
        #[arg(long)]
        replay_bytes: u64,
        #[arg(last = true, required = true)]
        program: Vec<OsString>,
    },
}

/// Runs a `terminal-session` subcommand and returns the process exit code.
pub fn run(command: TerminalSessionCommand) -> i32 {
    // The notices these processes write into the terminal. A holder keeps
    // the language of the attach that started it.
    crate::display_language::install_from_environment();
    let (id, nonce) = match &command {
        TerminalSessionCommand::Attach { id, nonce, .. } => (id, Some(nonce)),
        TerminalSessionCommand::Kill { id } | TerminalSessionCommand::Holder { id, .. } => {
            (id, None)
        }
    };
    let validation =
        validate_session_id(id).and_then(|()| nonce.map_or(Ok(()), |nonce| validate_nonce(nonce)));
    if let Err(error) = validation {
        print_error(&format!("{error:#}"));
        return EXIT_USAGE;
    }

    #[cfg(unix)]
    {
        posix::run(command)
    }
    #[cfg(not(unix))]
    {
        print_error("terminal sessions are not supported on this platform");
        EXIT_FAILURE
    }
}

/// Ends the shell kept for `session_id`, if any (`KillTerminalSession`).
#[cfg_attr(not(unix), allow(unused_variables))]
pub async fn kill(session_id: String, cx: &gpui::AsyncApp) -> Result<()> {
    validate_session_id(&session_id)?;
    #[cfg(unix)]
    {
        use gpui::AppContext as _;
        cx.background_spawn(async move {
            posix::kill_session(&posix::sessions_directory(), &session_id)
        })
        .await
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

fn print_error(message: &str) {
    use std::io::Write as _;
    let mut stderr = std::io::stderr().lock();
    stderr
        .write_all(format!("{message}\n").as_bytes())
        .and_then(|()| stderr.flush())
        .ok();
}

pub(crate) fn validate_session_id(id: &str) -> Result<()> {
    let valid = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        bail!("invalid terminal session id {id:?}: expected 1 to 64 of [A-Za-z0-9_-]");
    }
    Ok(())
}

fn validate_nonce(nonce: &str) -> Result<()> {
    let valid =
        (8..=64).contains(&nonce.len()) && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric());
    if !valid {
        bail!("invalid nonce: expected 8 to 64 of [A-Za-z0-9]");
    }
    Ok(())
}

/// How an attach that ends with `reason` (on stderr, with a failure exit
/// code either way) logs it. A session ended on purpose is how a closed tab
/// ends it, with its attach usually still there, which is no failure.
#[cfg(any(unix, test))]
fn attach_failure_log(id: &str, reason: &str) -> (log::Level, String) {
    if reason == SESSION_ENDED_REASON {
        (
            log::Level::Info,
            format!("terminal session {id} ended by kill"),
        )
    } else {
        (
            log::Level::Warn,
            format!("attaching to terminal session {id} failed: {reason}"),
        )
    }
}

/// How long a holder that no attach has got through to waits for one. Its
/// shell has not started, so there is nothing to keep: an attach that failed
/// on the way (one speaking another protocol version, or one whose connection
/// dropped at once) would otherwise leave it waiting for the whole detached
/// timeout. Longer than an attach takes to start a holder and introduce
/// itself.
#[cfg(any(unix, test))]
const UNATTACHED_HOLDER_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a holder with nothing attached keeps going: `configured`, the
/// detached timeout (`None` = forever), once its shell has started, and
/// otherwise no longer than `unattached` (see [`UNATTACHED_HOLDER_TIMEOUT`]).
#[cfg(any(unix, test))]
fn holder_detached_timeout(
    configured: Option<Duration>,
    shell_started: bool,
    unattached: Duration,
) -> Option<Duration> {
    if shell_started {
        return configured;
    }
    Some(configured.map_or(unattached, |configured| configured.min(unattached)))
}

#[cfg(any(unix, test))]
fn clamp_replay_bytes(requested: u64) -> usize {
    usize::try_from(requested.min(MAX_REPLAY_BYTES)).unwrap_or(usize::MAX)
}

#[cfg(any(unix, test))]
fn replay_begin_marker(nonce: &str) -> Vec<u8> {
    format!("\x1b]7719;replay-begin;{nonce}\x07").into_bytes()
}

#[cfg(any(unix, test))]
fn replay_end_marker(nonce: &str) -> Vec<u8> {
    format!("\x1b]7719;replay-end;{nonce}\x07").into_bytes()
}

#[cfg(any(unix, test))]
fn shell_exited_while_detached_notice(status: wire::ShellStatus) -> String {
    format!(
        "\r\n[{}]\r\n",
        i18n::t!(
            "The shell exited while disconnected (exit code {code})",
            code = status.displayed_code()
        )
    )
}

/// The file name (without extension) of a session's socket and lock. Hashed
/// so that the socket path stays short (see [`fits_unix_socket_path`]); the
/// host is part of it because a home directory shared over NFS is shared by
/// hosts that each run their own holders.
#[cfg(any(unix, test))]
fn session_file_stem(host_key: &str, id: &str) -> String {
    // FNV-1a: stable across Rust releases, unlike `DefaultHasher`, which
    // matters because an attach must find a holder started by an older build.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in host_key.bytes().chain([0]).chain(id.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("t-{hash:016x}")
}

/// Whether `path` fits in `sockaddr_un::sun_path` (with its terminating NUL):
/// 108 bytes on Linux, 104 on macOS.
#[cfg(any(unix, test))]
fn fits_unix_socket_path(path: &Path) -> bool {
    const SUN_PATH_BYTES: usize = if cfg!(target_os = "macos") { 104 } else { 108 };
    path.as_os_str().len() < SUN_PATH_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::ShellStatus;

    #[test]
    fn session_ids_are_checked() {
        assert!(validate_session_id("0b6f1c9e-8d7a-4b5c-9e2f-1a2b3c4d5e6f").is_ok());
        assert!(validate_session_id("a").is_ok());
        assert!(validate_session_id("A_z-9").is_ok());
        assert!(validate_session_id(&"x".repeat(64)).is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id(&"x".repeat(65)).is_err());
        assert!(validate_session_id("../etc").is_err());
        assert!(validate_session_id("a b").is_err());
        assert!(validate_session_id("a/b").is_err());
        assert!(validate_session_id("ä").is_err());
    }

    #[test]
    fn nonces_are_checked() {
        assert!(validate_nonce("abcdEFGH").is_ok());
        assert!(validate_nonce(&"a1".repeat(32)).is_ok());
        assert!(validate_nonce("abcdEFG").is_err());
        assert!(validate_nonce(&"a".repeat(65)).is_err());
        assert!(validate_nonce("abcd-EFGH").is_err());
        assert!(validate_nonce("abcd;EFGH").is_err());
    }

    #[test]
    fn a_holder_no_attach_got_through_to_ends_soon() {
        let day = Duration::from_secs(24 * 60 * 60);
        assert_eq!(
            holder_detached_timeout(Some(day), true, UNATTACHED_HOLDER_TIMEOUT),
            Some(day)
        );
        assert_eq!(
            holder_detached_timeout(None, true, UNATTACHED_HOLDER_TIMEOUT),
            None,
            "a started shell kept forever stays"
        );
        assert_eq!(
            holder_detached_timeout(Some(day), false, UNATTACHED_HOLDER_TIMEOUT),
            Some(UNATTACHED_HOLDER_TIMEOUT)
        );
        assert_eq!(
            holder_detached_timeout(None, false, UNATTACHED_HOLDER_TIMEOUT),
            Some(UNATTACHED_HOLDER_TIMEOUT),
            "even with no detached timeout, a holder without a shell ends"
        );
        let short = Duration::from_secs(5);
        assert_eq!(
            holder_detached_timeout(Some(short), false, UNATTACHED_HOLDER_TIMEOUT),
            Some(short)
        );
    }

    #[test]
    fn an_attach_ended_by_kill_is_not_logged_as_a_failure() {
        let (level, message) = attach_failure_log("tab", SESSION_ENDED_REASON);
        assert_eq!(level, log::Level::Info);
        assert_eq!(message, "terminal session tab ended by kill");

        let (level, message) = attach_failure_log("tab", "could not start the holder");
        assert_eq!(level, log::Level::Warn);
        assert_eq!(
            message,
            "attaching to terminal session tab failed: could not start the holder"
        );
    }

    #[test]
    fn invalid_arguments_exit_with_2() {
        let command = TerminalSessionCommand::Kill {
            id: "no/slashes".to_string(),
        };
        assert_eq!(run(command), EXIT_USAGE);
        let command = TerminalSessionCommand::Attach {
            id: "valid-id".to_string(),
            nonce: "short".to_string(),
            resume: false,
            detached_timeout_secs: 1,
            replay_bytes: 1,
            program: vec!["sh".into()],
        };
        assert_eq!(run(command), EXIT_USAGE);
    }

    #[test]
    fn the_command_line_is_the_contract() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct Cli {
            #[command(subcommand)]
            command: TerminalSessionCommand,
        }

        let cli = Cli::try_parse_from([
            "remote_server",
            "attach",
            "--id",
            "abc",
            "--nonce",
            "abcdefgh",
            "--resume",
            "--detached-timeout-secs",
            "0",
            "--replay-bytes",
            "100",
            "--",
            "/bin/bash",
            "-l",
        ])
        .expect("attach parses");
        match cli.command {
            TerminalSessionCommand::Attach {
                id,
                nonce,
                resume,
                detached_timeout_secs,
                replay_bytes,
                program,
            } => {
                assert_eq!(id, "abc");
                assert_eq!(nonce, "abcdefgh");
                assert!(resume);
                assert_eq!(detached_timeout_secs, 0);
                assert_eq!(replay_bytes, 100);
                assert_eq!(program, vec![OsString::from("/bin/bash"), "-l".into()]);
            }
            other => panic!("parsed as {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "remote_server",
            "attach",
            "--id",
            "abc",
            "--nonce",
            "abcdefgh",
            "--",
            "sh",
        ])
        .expect("attach with defaults parses");
        match cli.command {
            TerminalSessionCommand::Attach {
                resume,
                detached_timeout_secs,
                replay_bytes,
                ..
            } => {
                assert!(!resume);
                assert_eq!(detached_timeout_secs, 86_400);
                assert_eq!(replay_bytes, 4 * 1024 * 1024);
            }
            other => panic!("parsed as {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["remote_server", "kill", "--id", "abc"]).expect("kill parses");
        assert!(matches!(cli.command, TerminalSessionCommand::Kill { id } if id == "abc"));

        let missing_program = Cli::try_parse_from([
            "remote_server",
            "attach",
            "--id",
            "a",
            "--nonce",
            "abcdefgh",
        ]);
        let error = missing_program.err().expect("the program is required");
        assert_eq!(error.exit_code(), EXIT_USAGE);
    }

    #[test]
    fn replay_bytes_are_clamped() {
        assert_eq!(clamp_replay_bytes(0), 0);
        assert_eq!(clamp_replay_bytes(1000), 1000);
        assert_eq!(clamp_replay_bytes(MAX_REPLAY_BYTES), 64 * 1024 * 1024);
        assert_eq!(clamp_replay_bytes(u64::MAX), 64 * 1024 * 1024);
    }

    #[test]
    fn markers_are_the_contract() {
        assert_eq!(
            replay_begin_marker("Nonce123"),
            b"\x1b]7719;replay-begin;Nonce123\x07".to_vec()
        );
        assert_eq!(
            replay_end_marker("Nonce123"),
            b"\x1b]7719;replay-end;Nonce123\x07".to_vec()
        );
        assert_eq!(
            shell_exited_while_detached_notice(ShellStatus::Exited(3)),
            "\r\n[The shell exited while disconnected (exit code 3)]\r\n"
        );
        assert_eq!(
            shell_exited_while_detached_notice(ShellStatus::Signaled(9)),
            "\r\n[The shell exited while disconnected (exit code 137)]\r\n"
        );
        assert_eq!(
            taken_over_notice(),
            "\r\n[Another connection took over this shell]\r\n"
        );
        assert_eq!(EXIT_TAKEN_OVER, 253);
        assert_eq!(EXIT_DETACHED, 255);
        assert!(
            resumed_without_shell_notice().contains(
                "\r\n[The previous shell was no longer running. Starting a new shell]\r\n"
            )
        );
        assert!(resumed_without_shell_notice().starts_with("\x1b[0m"));
        assert!(resumed_without_shell_notice().ends_with("\x1b[0m"));
        assert!(truncated_replay_notice().starts_with("\x1b[0m[… Older output was omitted …]\r\n"));
    }

    #[test]
    fn session_file_names_are_stable_and_short() {
        let stem = session_file_stem("machine", "0b6f1c9e-8d7a-4b5c-9e2f-1a2b3c4d5e6f");
        // A holder started by an older build must still be found, so the
        // name must never change for the same input.
        assert_eq!(
            stem,
            session_file_stem("machine", "0b6f1c9e-8d7a-4b5c-9e2f-1a2b3c4d5e6f")
        );
        assert_eq!(session_file_stem("", ""), "t-af63bd4c8601b7df");
        assert_eq!(stem.len(), 18);
        assert_ne!(
            stem,
            session_file_stem("other machine", "0b6f1c9e-8d7a-4b5c-9e2f-1a2b3c4d5e6f")
        );
        assert_ne!(session_file_stem("ab", "c"), session_file_stem("a", "bc"));
    }

    #[test]
    fn long_socket_paths_are_noticed() {
        let short = Path::new(
            "/home/user/.local/share/zed/server_state/terminal_sessions/t-0123456789abcdef.sock",
        );
        assert!(fits_unix_socket_path(short));
        let long = Path::new("/").join("x".repeat(120));
        assert!(!fits_unix_socket_path(&long));
        let limit = if cfg!(target_os = "macos") { 103 } else { 107 };
        assert!(fits_unix_socket_path(
            &Path::new("/").join("x".repeat(limit - 1))
        ));
        assert!(!fits_unix_socket_path(
            &Path::new("/").join("x".repeat(limit))
        ));
    }
}
