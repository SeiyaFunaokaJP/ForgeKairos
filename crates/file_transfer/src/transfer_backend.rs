//! ADR-F28: the transfer engine behind the WinSCP-style file transfer screen.
//!
//! Every remote operation is carried out by a command that ships with the OS —
//! OpenSSH's `sftp` / `scp` for the SSH protocols and `curl` for FTP / FTPS —
//! so no SFTP or FTP implementation, key handling or host-key store has to be
//! maintained here. SSH authentication (passphrase, password and the first
//! host-key confirmation) goes through the same askpass helper Zed's remote
//! connections use.
//!
//! Everything a transfer does is started by the user pressing a button in the
//! modal, which ADR-F18 / F19 classify the same way as typing into a terminal:
//! **not** subject to the egress allow list, and deliberately not reachable
//! from an agent tool.
//!
//! The module is split so the fiddly parts are testable without a server:
//! parsing (`parse_*`) and command-line assembly (`*_args`, `curl_url`) are
//! pure functions, and process spawning sits behind [`ExecHost`] — which also
//! lets the "local" side of the screen be the *remote SSH host* rather than
//! the machine Zed runs on (see the trait's comment).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use askpass::{AskPassDelegate, AskPassResult, AskPassSession};
use fs::{Fs, RemoveOptions, RenameOptions};
use futures::{AsyncWriteExt as _, FutureExt as _, StreamExt as _, select_biased};
use gpui::{App, AppContext as _, AsyncApp, SharedString, Task, WeakEntity};
use project::{
    Project,
    remote_exec::{
        RemoteDirectoryEntry, RemoteEntryKind, RemoteFileOperation, TransferAskPass,
        TransferCommand, TransferProgram,
    },
};
use terminal::{
    quick_connect_credentials::{AskPassPrompt, classify_prompt},
    terminal_macro::ConnectionProtocol,
};
use util::ResultExt as _;

/// How the user reaches the remote host. SCP has no listing or management
/// commands of its own, so it borrows SFTP's (see [`TransferBackend::list`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransferProtocol {
    #[default]
    Sftp,
    Scp,
    Ftp,
    Ftps,
}

impl TransferProtocol {
    pub const ALL: [TransferProtocol; 4] = [
        TransferProtocol::Sftp,
        TransferProtocol::Scp,
        TransferProtocol::Ftp,
        TransferProtocol::Ftps,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TransferProtocol::Sftp => "SFTP",
            TransferProtocol::Scp => "SCP",
            TransferProtocol::Ftp => "FTP",
            TransferProtocol::Ftps => "FTPS",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            TransferProtocol::Sftp | TransferProtocol::Scp => 22,
            TransferProtocol::Ftp => 21,
            TransferProtocol::Ftps => 990,
        }
    }

    /// The name [`Self::from_name`] reads back, as written into a saved
    /// connection's `transfer_protocol` and into `terminal::OpenFileTransfer`.
    pub fn name(self) -> &'static str {
        match self {
            TransferProtocol::Sftp => "sftp",
            TransferProtocol::Scp => "scp",
            TransferProtocol::Ftp => "ftp",
            TransferProtocol::Ftps => "ftps",
        }
    }

    /// Parses the `protocol` field of `terminal::OpenFileTransfer`. Unknown
    /// values are rejected rather than guessed at, so a typo in a keymap falls
    /// back to the default instead of silently connecting over plain FTP.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "sftp" => Some(TransferProtocol::Sftp),
            "scp" => Some(TransferProtocol::Scp),
            "ftp" => Some(TransferProtocol::Ftp),
            "ftps" => Some(TransferProtocol::Ftps),
            _ => None,
        }
    }

    pub fn is_ssh(self) -> bool {
        matches!(self, TransferProtocol::Sftp | TransferProtocol::Scp)
    }

    /// Whether the protocol takes a user name and password rather than an SSH
    /// identity, and therefore needs the password field in the modal.
    pub fn uses_password(self) -> bool {
        matches!(self, TransferProtocol::Ftp | TransferProtocol::Ftps)
    }
}

// -------------------------------------------------------------------------
// Remembered passwords
// -------------------------------------------------------------------------

/// The quick-connect credential keys a transfer target can be answered from,
/// most specific first.
///
/// SFTP and SCP talk to the SSH daemon on the very port the transfer uses, so
/// their key is exact. FTP has no quick-connect protocol of its own — ADR-F28
/// already pairs it with Telnet ([`crate::target_from_saved_connection`]),
/// because a device that speaks Telnet is the kind that also speaks FTP and
/// its Telnet password is normally its FTP password. That pairing rewrites the
/// port to FTP's, which is why Telnet's own port is tried after the exact one.
pub fn credential_lookups(protocol: TransferProtocol, port: u16) -> Vec<(ConnectionProtocol, u16)> {
    match protocol {
        TransferProtocol::Sftp | TransferProtocol::Scp => vec![(ConnectionProtocol::Ssh, port)],
        TransferProtocol::Ftp | TransferProtocol::Ftps => {
            let telnet_port = ConnectionProtocol::Telnet.default_port();
            let mut keys = vec![(ConnectionProtocol::Telnet, port)];
            if port != telnet_port {
                keys.push((ConnectionProtocol::Telnet, telnet_port));
            }
            keys
        }
    }
}

/// What the transfer screen does with one of OpenSSH's askpass prompts while a
/// password is remembered for the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredPasswordUse {
    /// Hand the remembered password over without troubling the user.
    Answer,
    /// `ssh` asked for the password a second time, so the remembered one was
    /// refused. Answering again would only spend another attempt on the same
    /// wrong secret.
    Rejected,
    /// A host key confirmation, a key passphrase, or something unrecognized.
    /// Only a person can answer these — a first-contact host key in particular
    /// must never be accepted on the user's behalf.
    AskUser,
}

/// Decides one prompt. Unlike the quick-connect terminal's policy, which has
/// nowhere to fall back to and therefore fails everything after the first
/// password, this screen can put the prompt on the user's screen — so a
/// refused password turns into a prompt rather than a dead connection.
pub fn decide_stored_password_use(prompt: &str, already_answered: bool) -> StoredPasswordUse {
    match classify_prompt(prompt) {
        AskPassPrompt::Password if already_answered => StoredPasswordUse::Rejected,
        AskPassPrompt::Password => StoredPasswordUse::Answer,
        AskPassPrompt::Passphrase | AskPassPrompt::HostKey | AskPassPrompt::Other => {
            StoredPasswordUse::AskUser
        }
    }
}

/// How the transfer screen asks the user to answer one of OpenSSH's askpass
/// prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskPassAnswerMode {
    /// The first-contact host key question, answered with 「はい」「いいえ」
    /// buttons. It is a two-way decision, and a text field only invites the
    /// user to type something OpenSSH reads as "no" (第 12 便).
    Confirm,
    /// A masked field: the answer is a secret.
    Secret,
    /// A plain field. A user name is not a secret, and masking it makes it
    /// impossible to check before sending.
    PlainText,
}

/// What OpenSSH is answered with when the user presses 「はい」/「いいえ」 on a
/// host key question. `yes` is the literal `ssh` expects; anything else it
/// treats as a refusal, so 「いいえ」 sends the equally literal `no`.
pub const HOST_KEY_ACCEPTED: &str = "yes";
pub const HOST_KEY_REFUSED: &str = "no";

pub fn askpass_answer_mode(prompt: &str) -> AskPassAnswerMode {
    match classify_prompt(prompt) {
        AskPassPrompt::HostKey => AskPassAnswerMode::Confirm,
        AskPassPrompt::Password | AskPassPrompt::Passphrase => AskPassAnswerMode::Secret,
        // An unrecognized prompt is masked unless it plainly asks who the user
        // is: it is more likely to want a one-time code than a name, and
        // masking the wrong one costs only convenience.
        AskPassPrompt::Other => {
            let prompt = prompt.to_ascii_lowercase();
            if prompt.contains("username")
                || prompt.contains("user name")
                || prompt.contains("login")
            {
                AskPassAnswerMode::PlainText
            } else {
                AskPassAnswerMode::Secret
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteTarget {
    pub protocol: TransferProtocol,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
}

impl RemoteTarget {
    /// `user@host` or bare `host`, as OpenSSH wants it.
    pub fn ssh_destination(&self) -> String {
        match self
            .user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {}:{}",
            self.protocol.label(),
            self.ssh_destination(),
            self.port
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
}

impl EntryKind {
    pub fn is_directory(self) -> bool {
        self == EntryKind::Directory
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    /// Already formatted for display; listing formats differ too much between
    /// servers to be worth normalizing into a timestamp.
    pub modified: Option<String>,
}

/// Directories first, then names case-insensitively, which is how every file
/// manager the user is likely to compare this with orders a pane.
pub fn sort_entries(entries: &mut [DirectoryEntry]) {
    entries.sort_by(|left, right| {
        left.kind
            .is_directory()
            .cmp(&right.kind.is_directory())
            .reverse()
            .then_with(|| {
                left.name
                    .to_lowercase()
                    .cmp(&right.name.to_lowercase())
                    .then_with(|| left.name.cmp(&right.name))
            })
    });
}

// -------------------------------------------------------------------------
// Paths and validation
// -------------------------------------------------------------------------

/// Same character set the quick connect form accepts, plus what an IPv6
/// literal needs. The command line is assembled without a shell, but a host
/// name is also pasted into `user@host` and into a URL.
pub fn validate_host(host: &str) -> Result<(), &'static str> {
    if host.trim().is_empty() {
        return Err(i18n::t!("Enter a host name or an IP address."));
    }
    if host.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':' | '[' | ']')
    }) {
        Ok(())
    } else {
        Err(i18n::t!(
            "The host name contains characters that cannot be used."
        ))
    }
}

pub fn validate_user(user: &str) -> Result<(), &'static str> {
    if user.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '@')
    }) {
        Ok(())
    } else {
        Err(i18n::t!(
            "The user name contains characters that cannot be used."
        ))
    }
}

/// Remote paths are always absolute and `/`-separated. The rejected characters
/// are the ones that would have to be escaped inside an `sftp` batch argument
/// or inside an FTP protocol command, and no remote file system needs them.
pub fn validate_remote_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err(i18n::t!("Enter the remote path."));
    }
    if !path.starts_with('/') {
        return Err(i18n::t!("The remote path must start with /."));
    }
    if path
        .chars()
        .any(|character| matches!(character, '"' | '\\') || character.is_control())
    {
        return Err(i18n::t!(
            "The remote path contains characters that cannot be used."
        ));
    }
    Ok(())
}

/// A single path component typed by the user (new folder, rename target).
pub fn validate_entry_name(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err(i18n::t!("Enter a name."));
    }
    if name == "." || name == ".." {
        return Err(i18n::t!("That name cannot be used."));
    }
    if name.chars().any(|character| {
        matches!(
            character,
            '/' | '\\' | '"' | ':' | '*' | '?' | '<' | '>' | '|'
        ) || character.is_control()
    }) {
        return Err(i18n::t!(
            "The name contains characters that cannot be used."
        ));
    }
    Ok(())
}

pub fn normalize_remote_dir(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let mut with_root = String::with_capacity(trimmed.len() + 1);
    if !trimmed.starts_with('/') {
        with_root.push('/');
    }
    for character in trimmed.chars() {
        if character == '/' && with_root.ends_with('/') {
            continue;
        }
        with_root.push(character);
    }
    let collapsed = with_root.trim_end_matches('/');
    if collapsed.is_empty() {
        "/".to_string()
    } else {
        collapsed.to_string()
    }
}

pub fn remote_join(directory: &str, name: &str) -> String {
    let directory = normalize_remote_dir(directory);
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

pub fn remote_parent(directory: &str) -> String {
    let directory = normalize_remote_dir(directory);
    match directory.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => directory[..index].to_string(),
    }
}

// -------------------------------------------------------------------------
// Listing parsers
// -------------------------------------------------------------------------

/// `sftp -b` echoes each command it reads back to stdout with this prefix, so
/// the echoes have to be dropped before the listing is parsed.
const SFTP_ECHO_PREFIX: &str = "sftp>";

/// Splits off `count` whitespace-separated fields and returns them together
/// with the rest of the line, so a name containing spaces survives verbatim.
fn split_leading_fields(line: &str, count: usize) -> Option<(Vec<&str>, &str)> {
    let mut fields = Vec::with_capacity(count);
    let mut rest = line;
    for _ in 0..count {
        let start = rest.find(|character: char| !character.is_whitespace())?;
        let after_start = &rest[start..];
        let end = after_start
            .find(char::is_whitespace)
            .unwrap_or(after_start.len());
        fields.push(&after_start[..end]);
        rest = &after_start[end..];
    }
    let remainder_start = rest.find(|character: char| !character.is_whitespace())?;
    Some((fields, &rest[remainder_start..]))
}

pub fn parse_sftp_listing(stdout: &str) -> Vec<DirectoryEntry> {
    let without_echoes = stdout
        .lines()
        .filter(|line| !line.trim_start().starts_with(SFTP_ECHO_PREFIX))
        .collect::<Vec<_>>()
        .join("\n");
    parse_unix_long_listing(&without_echoes)
}

/// `ls -l` in the form both OpenSSH's `sftp` and most FTP servers' `LIST`
/// produce:
///
/// ```text
/// drwxr-xr-x  2 user group 4096 Sep  6 12:34 name
/// -rw-r--r--  1 user group 1234 Jan 10  2024 name with spaces.txt
/// lrwxrwxrwx  1 user group   11 Sep  6 12:34 link -> /target
/// ```
pub fn parse_unix_long_listing(text: &str) -> Vec<DirectoryEntry> {
    text.lines()
        .filter_map(parse_unix_long_listing_line)
        .filter(|entry| entry.name != "." && entry.name != "..")
        .collect()
}

fn parse_unix_long_listing_line(line: &str) -> Option<DirectoryEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (fields, name) = split_leading_fields(line, 8)?;

    let permissions = fields.first()?;
    if permissions.len() < 10 {
        return None;
    }
    let kind = match permissions.chars().next()? {
        'd' => EntryKind::Directory,
        'l' => EntryKind::Symlink,
        '-' | 'b' | 'c' | 'p' | 's' => EntryKind::File,
        _ => return None,
    };

    // A symlink line ends with ` -> target`; the pane shows the link itself.
    let name = match kind {
        EntryKind::Symlink => name.split(" -> ").next().unwrap_or(name),
        _ => name,
    };
    // Win32-OpenSSH's `sftp` prints `ls -l -a "/home"` entries as
    // `/home/.`, `/home/ubuntu`; OpenBSD's strips the directory. Names cannot
    // contain `/`, so the last component is the entry either way.
    let name = name.rsplit('/').next().unwrap_or(name);
    if name.is_empty() {
        return None;
    }

    let size = fields.get(4).and_then(|field| field.parse::<u64>().ok());
    let modified = match (fields.get(5), fields.get(6), fields.get(7)) {
        (Some(month), Some(day), Some(time_or_year)) => {
            Some(format!("{month} {day} {time_or_year}"))
        }
        _ => None,
    };

    Some(DirectoryEntry {
        name: name.to_string(),
        kind,
        size: if kind == EntryKind::Directory {
            None
        } else {
            size
        },
        modified,
    })
}

/// RFC 3659 `MLSD`: semicolon-separated facts, a single space, then the name.
///
/// ```text
/// type=dir;modify=20240110123456;size=4096; documents
/// type=file;size=1234;modify=20240110123456; notes.txt
/// ```
pub fn parse_mlsd_listing(text: &str) -> Vec<DirectoryEntry> {
    text.lines()
        .filter_map(parse_mlsd_line)
        .filter(|entry| entry.name != "." && entry.name != "..")
        .collect()
}

fn parse_mlsd_line(line: &str) -> Option<DirectoryEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (facts, name) = line.split_once(' ')?;
    if name.is_empty() {
        return None;
    }
    let mut kind = None;
    let mut size = None;
    let mut modified = None;
    for fact in facts.split(';') {
        let Some((key, value)) = fact.split_once('=') else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => {
                let value = value.to_ascii_lowercase();
                kind = match value.as_str() {
                    "dir" => Some(EntryKind::Directory),
                    // `cdir` / `pdir` are `.` and `..`.
                    "cdir" | "pdir" => return None,
                    "file" => Some(EntryKind::File),
                    other if other.starts_with("os.unix=slink") => Some(EntryKind::Symlink),
                    _ => None,
                }
            }
            "size" | "sizd" => size = value.parse::<u64>().ok(),
            "modify" => modified = format_mlsd_timestamp(value),
            _ => {}
        }
    }
    Some(DirectoryEntry {
        name: name.to_string(),
        kind: kind?,
        size,
        modified,
    })
}

/// `YYYYMMDDHHMMSS[.sss]` as sent by `MLSD`, shown the way the local pane
/// shows its own timestamps.
fn format_mlsd_timestamp(value: &str) -> Option<String> {
    let digits = value.split('.').next().unwrap_or(value);
    if digits.len() < 12 || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{} {}:{}",
        &digits[0..4],
        &digits[4..6],
        &digits[6..8],
        &digits[8..10],
        &digits[10..12],
    ))
}

/// An FTP `LIST` response, which is either Unix `ls -l` or the MS-DOS format
/// IIS still emits:
///
/// ```text
/// 01-10-24  12:34PM       <DIR>          documents
/// 01-10-24  12:34PM               1234 notes.txt
/// ```
pub fn parse_ftp_listing(text: &str) -> Vec<DirectoryEntry> {
    let unix = parse_unix_long_listing(text);
    if !unix.is_empty() {
        return unix;
    }
    text.lines()
        .filter_map(parse_dos_listing_line)
        .filter(|entry| entry.name != "." && entry.name != "..")
        .collect()
}

fn parse_dos_listing_line(line: &str) -> Option<DirectoryEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (fields, rest) = split_leading_fields(line, 2)?;
    let (date, time) = (fields.first()?, fields.get(1)?);
    let uppercase_time = time.to_ascii_uppercase();
    if !date.contains('-') || !(uppercase_time.ends_with("AM") || uppercase_time.ends_with("PM")) {
        return None;
    }

    let (kind, size, name) = match rest.strip_prefix("<DIR>") {
        Some(after_marker) => (EntryKind::Directory, None, after_marker.trim_start()),
        None => {
            let (size_fields, name) = split_leading_fields(rest, 1)?;
            (
                EntryKind::File,
                size_fields
                    .first()
                    .and_then(|size| size.parse::<u64>().ok()),
                name,
            )
        }
    };
    if name.is_empty() {
        return None;
    }
    Some(DirectoryEntry {
        name: name.to_string(),
        kind,
        size,
        modified: Some(format!("{date} {time}")),
    })
}

// -------------------------------------------------------------------------
// Command lines
// -------------------------------------------------------------------------

/// Long enough that a busy host still answers, short enough that an
/// unreachable one does not leave a job spinning forever. `sftp`, `scp` and
/// curl enforce this themselves, so no timer has to race the process here.
const CONNECT_TIMEOUT_SECONDS: u32 = 15;

/// `sftp -b -`, reading its batch script from stdin.
///
/// `-b` implies `BatchMode=yes`, which would refuse every password prompt;
/// turning it back off is what lets the askpass helper answer passphrase,
/// password and host-key questions (ADR-F28).
pub fn sftp_args(target: &RemoteTarget) -> Vec<String> {
    vec![
        "-q".to_string(),
        "-o".to_string(),
        "BatchMode=no".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
        "-P".to_string(),
        target.port.to_string(),
        "-b".to_string(),
        "-".to_string(),
        target.ssh_destination(),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

/// `scp` command line.
///
/// `local_argument` is the source file when uploading and the destination
/// directory when downloading; [`TransferBackend::scp_local_argument`] decides
/// whether it is an absolute path or a bare name resolved against a working
/// directory, which is where the Windows drive-letter problem is handled.
pub fn scp_args(
    target: &RemoteTarget,
    direction: TransferDirection,
    recursive: bool,
    local_argument: &str,
    remote_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
        "-P".to_string(),
        target.port.to_string(),
    ];
    if recursive {
        args.push("-r".to_string());
    }
    let destination = target.ssh_destination();
    match direction {
        TransferDirection::Upload => {
            args.push(local_argument.to_string());
            args.push(format!(
                "{destination}:{}",
                scp_quote_remote(&format!("{}/", normalize_remote_dir(remote_path)))
            ));
        }
        TransferDirection::Download => {
            args.push(format!("{destination}:{}", scp_quote_remote(remote_path)));
            args.push(local_argument.to_string());
        }
    }
    args
}

/// `scp` hands the remote path to a shell on the far side (its legacy
/// protocol mode does so literally), so a path with spaces or shell
/// metacharacters has to arrive quoted. Paths that need no quoting are left
/// alone, because OpenSSH 9's SFTP-backed `scp` would otherwise take the
/// quotes as part of the name.
pub fn scp_quote_remote(path: &str) -> String {
    let needs_quoting = path.is_empty()
        || path.chars().any(|character| {
            character.is_ascii()
                && !(character.is_ascii_alphanumeric()
                    || matches!(
                        character,
                        '/' | '.' | '_' | '-' | '+' | '=' | ',' | '%' | '@'
                    ))
        });
    if needs_quoting {
        format!("'{path}'")
    } else {
        path.to_string()
    }
}

/// The FTP URL curl is pointed at.
///
/// The path is deliberately single-slashed: curl resolves `ftp://host/x`
/// relative to the login directory, which is what the remote pane's `/` means
/// for FTP. (`ftp://host//x` would mean the server's real root, which many
/// accounts cannot even see.)
pub fn curl_url(target: &RemoteTarget, path: &str, trailing_slash: bool) -> String {
    let scheme = match target.protocol {
        TransferProtocol::Ftps => "ftps",
        _ => "ftp",
    };
    let encoded = percent_encode_remote_path(path);
    let slash = if trailing_slash && !encoded.ends_with('/') {
        "/"
    } else {
        ""
    };
    format!("{scheme}://{}:{}{encoded}{slash}", target.host, target.port)
}

/// Percent-encodes everything a URL path cannot carry literally, leaving `/`
/// as the separator. curl decodes this again before it sends `CWD` / `RETR`,
/// so a name with spaces or `#` reaches the server intact.
pub fn percent_encode_remote_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        let character = *byte as char;
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_' | '~' | '/') {
            encoded.push(character);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Wraps an argument for an `sftp` batch command. Callers validate the path
/// first, so the quoting has nothing left to escape.
fn sftp_quote(path: &str) -> String {
    format!("\"{path}\"")
}

// -------------------------------------------------------------------------
// Execution hosts
// -------------------------------------------------------------------------

/// Builds the askpass delegate that answers SSH's prompts in the GUI. It is a
/// factory rather than a delegate because each spawned process needs its own
/// session, and a session cannot outlive the process it belongs to.
pub type AskPassFactory = Arc<dyn Fn(&mut AsyncApp) -> AskPassDelegate + Send + Sync>;

/// One invocation of a transfer program, as handed to an [`ExecHost`].
pub struct HostCommand {
    pub program: TransferProgram,
    /// Written to the process's stdin, which is then closed. `None` leaves
    /// stdin closed from the start.
    pub stdin: Option<String>,
    pub args: Vec<String>,
    pub working_directory: Option<String>,
    /// Whether OpenSSH's passphrase / password / host-key questions should be
    /// answered through the GUI.
    pub wants_askpass: bool,
}

/// Which machine the transfer screen's "local" side is.
///
/// For a plain local project this is always the machine Zed runs on. For an
/// SSH remote project the remote host is offered too — and is the default,
/// because the project, its terminals and every path the user is looking at
/// live over there, so `/home/ubuntu` in the local pane has to mean the
/// remote's `/home/ubuntu` rather than a missing Windows directory (ADR-F28).
pub trait ExecHost: Send + Sync + 'static {
    /// Names this side in the pane header's selector.
    fn label(&self) -> SharedString;

    /// Whether this is the remote SSH host rather than the machine Zed runs
    /// on. Only `scp`'s local argument depends on it; see
    /// [`TransferBackend::scp_local_argument`].
    fn is_remote(&self) -> bool;

    /// Where the pane starts when this side is selected.
    fn root_directory(&self) -> String;

    fn join(&self, directory: &str, name: &str) -> String;

    /// `None` at the top of the tree.
    fn parent(&self, directory: &str) -> Option<String>;

    fn list_directory(
        &self,
        directory: String,
        cx: &mut AsyncApp,
    ) -> Task<Result<Vec<DirectoryEntry>>>;

    fn make_directory(&self, path: String, cx: &mut AsyncApp) -> Task<Result<()>>;

    fn rename(&self, from: String, to: String, cx: &mut AsyncApp) -> Task<Result<()>>;

    fn remove(&self, path: String, kind: EntryKind, cx: &mut AsyncApp) -> Task<Result<()>>;

    /// Runs one transfer program to completion and returns its stdout. A
    /// non-zero exit becomes an error carrying the program's own message.
    fn run(&self, command: HostCommand, cx: &mut AsyncApp) -> Task<Result<String>>;

    /// Stops anything still running, so a transfer cannot outlive the screen
    /// that started it. A no-op where the processes are this process's own
    /// children: dropping their task already kills them.
    fn cancel_running(&self, _cx: &mut App) {}
}

/// The machine Zed itself runs on: the transfer programs are its own child
/// processes and the pane reads its [`Fs`].
pub struct IdeExecHost {
    fs: Arc<dyn Fs>,
    root: String,
    askpass: AskPassFactory,
}

impl IdeExecHost {
    pub fn new(fs: Arc<dyn Fs>, root: String, askpass: AskPassFactory) -> Self {
        Self { fs, root, askpass }
    }
}

impl ExecHost for IdeExecHost {
    fn label(&self) -> SharedString {
        i18n::t!("This Computer").into()
    }

    fn is_remote(&self) -> bool {
        false
    }

    fn root_directory(&self) -> String {
        self.root.clone()
    }

    fn join(&self, directory: &str, name: &str) -> String {
        Path::new(directory)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn parent(&self, directory: &str) -> Option<String> {
        Path::new(directory)
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
    }

    fn list_directory(
        &self,
        directory: String,
        cx: &mut AsyncApp,
    ) -> Task<Result<Vec<DirectoryEntry>>> {
        let fs = self.fs.clone();
        cx.background_spawn(async move { list_local_directory(&fs, Path::new(&directory)).await })
    }

    fn make_directory(&self, path: String, cx: &mut AsyncApp) -> Task<Result<()>> {
        let fs = self.fs.clone();
        cx.background_spawn(async move { fs.create_dir(Path::new(&path)).await })
    }

    fn rename(&self, from: String, to: String, cx: &mut AsyncApp) -> Task<Result<()>> {
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            fs.rename(Path::new(&from), Path::new(&to), RenameOptions::default())
                .await
        })
    }

    fn remove(&self, path: String, kind: EntryKind, cx: &mut AsyncApp) -> Task<Result<()>> {
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            let options = RemoveOptions {
                recursive: true,
                ignore_if_not_exists: false,
            };
            let path = PathBuf::from(path);
            if kind.is_directory() {
                fs.remove_dir(&path, options).await
            } else {
                fs.remove_file(&path, options).await
            }
        })
    }

    fn run(&self, command: HostCommand, cx: &mut AsyncApp) -> Task<Result<String>> {
        let askpass = self.askpass.clone();
        cx.spawn(async move |cx| run_on_this_machine(command, askpass, cx).await)
    }
}

/// The SSH host the project is connected to. The pane reads that host's own
/// file system over the project's RPC channel and the transfer programs run
/// over there, so a transfer between the project's host and a third machine
/// never passes through the IDE (ADR-F28).
///
/// A fresh `ssh` per operation was rejected: Windows has no ControlMaster, so
/// every listing would re-authenticate.
pub struct RemoteExecHost {
    project: WeakEntity<Project>,
    label: SharedString,
    root: String,
    askpass: TransferAskPass,
}

impl RemoteExecHost {
    pub fn new(
        project: WeakEntity<Project>,
        display_name: &str,
        root: String,
        askpass: TransferAskPass,
    ) -> Self {
        Self {
            project,
            label: i18n::t!("This Remote ({display_name})").into(),
            root,
            askpass,
        }
    }
}

impl ExecHost for RemoteExecHost {
    fn label(&self) -> SharedString {
        self.label.clone()
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn root_directory(&self) -> String {
        self.root.clone()
    }

    fn join(&self, directory: &str, name: &str) -> String {
        remote_join(directory, name)
    }

    fn parent(&self, directory: &str) -> Option<String> {
        let directory = normalize_remote_dir(directory);
        (directory != "/").then(|| remote_parent(&directory))
    }

    fn list_directory(
        &self,
        directory: String,
        cx: &mut AsyncApp,
    ) -> Task<Result<Vec<DirectoryEntry>>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let entries = project
                .update(cx, |project, cx| {
                    project.list_transfer_directory(directory, cx)
                })?
                .await?;
            let mut entries = entries
                .into_iter()
                .map(remote_directory_entry)
                .collect::<Vec<_>>();
            sort_entries(&mut entries);
            Ok(entries)
        })
    }

    fn make_directory(&self, path: String, cx: &mut AsyncApp) -> Task<Result<()>> {
        self.file_operation(RemoteFileOperation::MakeDirectory { path }, cx)
    }

    fn rename(&self, from: String, to: String, cx: &mut AsyncApp) -> Task<Result<()>> {
        self.file_operation(RemoteFileOperation::Rename { from, to }, cx)
    }

    fn remove(&self, path: String, kind: EntryKind, cx: &mut AsyncApp) -> Task<Result<()>> {
        self.file_operation(
            RemoteFileOperation::Remove {
                path,
                recursive: kind.is_directory(),
            },
            cx,
        )
    }

    fn run(&self, command: HostCommand, cx: &mut AsyncApp) -> Task<Result<String>> {
        let project = self.project.clone();
        let askpass = self.askpass.clone();
        cx.spawn(async move |cx| {
            let output = project
                .update(cx, |project, cx| {
                    project.run_transfer_command(
                        TransferCommand {
                            program: command.program,
                            args: command.args,
                            stdin: command.stdin,
                            working_directory: command.working_directory,
                            wants_askpass: command.wants_askpass,
                        },
                        askpass,
                        cx,
                    )
                })?
                .await?;
            if output.cancelled {
                bail!(i18n::t!("Canceled."));
            }
            if !output.succeeded() {
                bail!(command_failure_message(&output.stderr, &output.stdout));
            }
            Ok(output.stdout)
        })
    }

    fn cancel_running(&self, cx: &mut App) {
        self.project
            .update(cx, |project, cx| project.cancel_transfer_commands(cx))
            .log_err();
    }
}

impl RemoteExecHost {
    fn file_operation(
        &self,
        operation: RemoteFileOperation,
        cx: &mut AsyncApp,
    ) -> Task<Result<()>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            project
                .update(cx, |project, cx| {
                    project.transfer_file_operation(operation, cx)
                })?
                .await
        })
    }
}

fn remote_directory_entry(entry: RemoteDirectoryEntry) -> DirectoryEntry {
    DirectoryEntry {
        name: entry.name,
        kind: match entry.kind {
            RemoteEntryKind::Directory => EntryKind::Directory,
            RemoteEntryKind::Symlink => EntryKind::Symlink,
            RemoteEntryKind::File => EntryKind::File,
        },
        size: entry.size,
        modified: entry.mtime_epoch_seconds.and_then(format_epoch_seconds),
    }
}

/// The remote host's timestamps, shown the way the local pane shows its own.
fn format_epoch_seconds(seconds: i64) -> Option<String> {
    Some(
        chrono::DateTime::from_timestamp(seconds, 0)?
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    )
}

/// Runs a transfer program as a child of this process, with Zed's askpass
/// helper wired up so passphrase, password and first-time host-key prompts
/// appear in the GUI exactly as they do for a remote project (ADR-F28).
async fn run_on_this_machine(
    command: HostCommand,
    askpass: AskPassFactory,
    cx: &mut AsyncApp,
) -> Result<String> {
    let mut process = util::command::new_command(ide_program_path(command.program));
    process
        .args(&command.args)
        .stdin(if command.stdin.is_some() {
            util::command::Stdio::piped()
        } else {
            util::command::Stdio::null()
        })
        .stdout(util::command::Stdio::piped())
        .stderr(util::command::Stdio::piped())
        .kill_on_drop(true);
    if let Some(working_directory) = &command.working_directory {
        process.current_dir(working_directory);
    }

    let mut session = if command.wants_askpass {
        let delegate = (askpass)(cx);
        let session = AskPassSession::new(cx.background_executor().clone(), delegate).await?;
        process
            .env("SSH_ASKPASS", session.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force");
        #[cfg(target_os = "windows")]
        process.env("ZED_ASKPASS_SOCKET", session.socket_path());
        Some(session)
    } else {
        None
    };

    let mut child = process
        .spawn()
        .with_context(|| i18n::t!("Could not run {program}.", program = command.program.name()))?;
    if let Some(stdin) = command.stdin {
        let mut handle = child
            .stdin
            .take()
            .context(i18n::t!("Could not open standard input"))?;
        handle.write_all(stdin.as_bytes()).await?;
        handle.flush().await?;
        drop(handle);
    }

    // No timeout is given to `run`: a transfer may legitimately go a long time
    // without prompting, and `ConnectTimeout` already bounds the
    // unreachable-host case.
    let output = match session.as_mut() {
        Some(session) => select_biased! {
            result = session.run(None).fuse() => {
                match result {
                    AskPassResult::CancelledByUser => bail!(i18n::t!("Authentication was canceled.")),
                    AskPassResult::Timedout => bail!(i18n::t!("The connection timed out.")),
                }
            }
            output = child.output().fuse() => output?,
        },
        None => child.output().await?,
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        bail!(command_failure_message(&stderr, &stdout));
    }
    Ok(stdout)
}

/// How this machine invokes each transfer program. Only curl needs pointing
/// at: Windows keeps it in System32, while its `sftp` and `scp` are on `PATH`.
fn ide_program_path(program: TransferProgram) -> PathBuf {
    match program {
        TransferProgram::Curl => curl_program(),
        program => PathBuf::from(program.name()),
    }
}

// -------------------------------------------------------------------------
// The transfer engine
// -------------------------------------------------------------------------

/// Guard against a pathological tree turning a single click into an unbounded
/// number of processes.
const MAX_WALKED_ENTRIES: usize = 2000;

pub struct TransferBackend {
    target: RemoteTarget,
    /// The machine the transfer programs run on, which is also the machine the
    /// "local" pane is showing.
    host: Arc<dyn ExecHost>,
    /// FTP only, held for the life of the connection and never written to
    /// disk (ADR-F28). `None` lets curl fall back to `.netrc`.
    password: Option<String>,
}

impl TransferBackend {
    pub fn new(target: RemoteTarget, host: Arc<dyn ExecHost>, password: Option<String>) -> Self {
        Self {
            target,
            host,
            password,
        }
    }

    pub fn target(&self) -> &RemoteTarget {
        &self.target
    }

    pub async fn list(&self, directory: &str, cx: &mut AsyncApp) -> Result<Vec<DirectoryEntry>> {
        let directory = normalize_remote_dir(directory);
        validate_remote_path(&directory).map_err(|message| anyhow!("{message}"))?;
        let mut entries = if self.target.protocol.is_ssh() {
            let stdout = self
                .run_sftp_script(None, format!("ls -l -a {}\n", sftp_quote(&directory)), cx)
                .await?;
            parse_sftp_listing(&stdout)
        } else {
            self.list_over_ftp(&directory, cx).await?
        };
        sort_entries(&mut entries);
        Ok(entries)
    }

    async fn list_over_ftp(
        &self,
        directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<Vec<DirectoryEntry>> {
        let url = curl_url(&self.target, directory, true);
        let mlsd = self
            .run_curl(
                None,
                vec!["-X".to_string(), "MLSD".to_string(), url.clone()],
                cx,
            )
            .await;
        match mlsd {
            Ok(stdout) if stdout.contains("type=") => Ok(parse_mlsd_listing(&stdout)),
            Ok(stdout) if stdout.trim().is_empty() => Ok(Vec::new()),
            // Servers without `MLSD` answer 500; fall back to `LIST`, which
            // every FTP server has.
            _ => {
                let stdout = self.run_curl(None, vec![url], cx).await?;
                Ok(parse_ftp_listing(&stdout))
            }
        }
    }

    pub async fn make_directory(&self, path: &str, cx: &mut AsyncApp) -> Result<()> {
        validate_remote_path(path).map_err(|message| anyhow!("{message}"))?;
        if self.target.protocol.is_ssh() {
            self.run_sftp_script(None, format!("mkdir {}\n", sftp_quote(path)), cx)
                .await?;
        } else {
            self.run_curl(
                None,
                vec![
                    "-Q".to_string(),
                    format!("MKD {path}"),
                    curl_url(&self.target, &remote_parent(path), true),
                ],
                cx,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str, cx: &mut AsyncApp) -> Result<()> {
        validate_remote_path(from).map_err(|message| anyhow!("{message}"))?;
        validate_remote_path(to).map_err(|message| anyhow!("{message}"))?;
        if self.target.protocol.is_ssh() {
            let script = format!("rename {} {}\n", sftp_quote(from), sftp_quote(to));
            self.run_sftp_script(None, script, cx).await?;
        } else {
            self.run_curl(
                None,
                vec![
                    "-Q".to_string(),
                    format!("RNFR {from}"),
                    "-Q".to_string(),
                    format!("RNTO {to}"),
                    curl_url(&self.target, &remote_parent(from), true),
                ],
                cx,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn remove(&self, path: &str, kind: EntryKind, cx: &mut AsyncApp) -> Result<()> {
        validate_remote_path(path).map_err(|message| anyhow!("{message}"))?;
        // Neither `sftp` nor FTP has a recursive delete, so the tree is walked
        // breadth-first here and removed in reverse (leaves first).
        let mut victims = vec![(path.to_string(), kind)];
        let mut index = 0;
        while index < victims.len() {
            let (current, current_kind) = victims[index].clone();
            index += 1;
            if !current_kind.is_directory() {
                continue;
            }
            for entry in self.list(&current, cx).await? {
                if victims.len() >= MAX_WALKED_ENTRIES {
                    bail!(i18n::t!(
                        "More than {limit} items would be deleted. \
                         Delete them in smaller batches.",
                        limit = MAX_WALKED_ENTRIES
                    ));
                }
                victims.push((remote_join(&current, &entry.name), entry.kind));
            }
        }

        if self.target.protocol.is_ssh() {
            let mut script = String::new();
            for (victim, victim_kind) in victims.iter().rev() {
                let command = if victim_kind.is_directory() {
                    "rmdir"
                } else {
                    "rm"
                };
                script.push_str(&format!("{command} {}\n", sftp_quote(victim)));
            }
            self.run_sftp_script(None, script, cx).await?;
        } else {
            let mut args = Vec::new();
            for (victim, victim_kind) in victims.iter().rev() {
                let command = if victim_kind.is_directory() {
                    "RMD"
                } else {
                    "DELE"
                };
                args.push("-Q".to_string());
                args.push(format!("{command} {victim}"));
            }
            args.push(curl_url(&self.target, &remote_parent(path), true));
            self.run_curl(None, args, cx).await?;
        }
        Ok(())
    }

    /// Sends `local_directory/name` into `remote_directory`.
    pub async fn upload(
        &self,
        local_directory: &str,
        name: &str,
        is_directory: bool,
        remote_directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let remote_directory = normalize_remote_dir(remote_directory);
        validate_remote_path(&remote_directory).map_err(|message| anyhow!("{message}"))?;
        validate_entry_name(name).map_err(|message| anyhow!("{message}"))?;
        match self.target.protocol {
            TransferProtocol::Sftp => {
                // The local name is relative to the process's working
                // directory, which keeps Windows path separators (an `sftp`
                // batch escape character) out of the script entirely.
                let flag = if is_directory { "-r " } else { "" };
                let script = format!(
                    "put {flag}{} {}\n",
                    sftp_quote(name),
                    sftp_quote(&remote_join(&remote_directory, name))
                );
                self.run_sftp_script(Some(local_directory.to_string()), script, cx)
                    .await?;
                Ok(())
            }
            TransferProtocol::Scp => {
                let (working_directory, local_argument) =
                    self.scp_local_argument(local_directory, Some(name));
                self.run_scp(
                    working_directory,
                    TransferDirection::Upload,
                    is_directory,
                    &local_argument,
                    &remote_directory,
                    cx,
                )
                .await
            }
            TransferProtocol::Ftp | TransferProtocol::Ftps => {
                self.upload_over_ftp(local_directory, name, is_directory, &remote_directory, cx)
                    .await
            }
        }
    }

    /// Fetches `remote_directory/name` into `local_directory`.
    pub async fn download(
        &self,
        remote_directory: &str,
        name: &str,
        is_directory: bool,
        local_directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let remote_directory = normalize_remote_dir(remote_directory);
        validate_entry_name(name).map_err(|message| anyhow!("{message}"))?;
        let remote_path = remote_join(&remote_directory, name);
        validate_remote_path(&remote_path).map_err(|message| anyhow!("{message}"))?;
        match self.target.protocol {
            TransferProtocol::Sftp => {
                let flag = if is_directory { "-r " } else { "" };
                let script = format!(
                    "get {flag}{} {}\n",
                    sftp_quote(&remote_path),
                    sftp_quote(name)
                );
                self.run_sftp_script(Some(local_directory.to_string()), script, cx)
                    .await?;
                Ok(())
            }
            TransferProtocol::Scp => {
                let (working_directory, local_argument) =
                    self.scp_local_argument(local_directory, None);
                self.run_scp(
                    working_directory,
                    TransferDirection::Download,
                    is_directory,
                    &local_argument,
                    &remote_path,
                    cx,
                )
                .await
            }
            TransferProtocol::Ftp | TransferProtocol::Ftps => {
                self.download_over_ftp(&remote_directory, name, is_directory, local_directory, cx)
                    .await
            }
        }
    }

    /// `scp`'s local-side argument, and the working directory to run it in.
    ///
    /// `scp` splits an argument into `host:path` at the first colon that has
    /// no `/` before it, so a Windows path would be split on its drive
    /// letter — hence the bare name plus a working directory there. On the
    /// remote host the paths are absolute and `/`-separated, so they can be
    /// passed as they are, which also survives a name that contains a colon.
    ///
    /// `name` is the file being uploaded; `None` means a download, whose local
    /// argument is the destination directory.
    fn scp_local_argument(
        &self,
        local_directory: &str,
        name: Option<&str>,
    ) -> (Option<String>, String) {
        match (self.host.is_remote(), name) {
            (true, Some(name)) => (None, self.host.join(local_directory, name)),
            (true, None) => (None, local_directory.to_string()),
            (false, Some(name)) => (Some(local_directory.to_string()), name.to_string()),
            (false, None) => (Some(local_directory.to_string()), ".".to_string()),
        }
    }

    /// curl transfers one file per invocation, so a directory is walked here
    /// and turned into `MKD` plus one upload per file (ADR-F28).
    async fn upload_over_ftp(
        &self,
        local_directory: &str,
        name: &str,
        is_directory: bool,
        remote_directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let mut pending = vec![(
            local_directory.to_string(),
            name.to_string(),
            remote_directory.to_string(),
            is_directory,
        )];
        let mut walked = 0;
        while let Some((local_parent, name, remote_parent_directory, entry_is_directory)) =
            pending.pop()
        {
            walked += 1;
            if walked > MAX_WALKED_ENTRIES {
                bail!(i18n::t!(
                    "More than {limit} files would be transferred. \
                     Transfer them in smaller batches.",
                    limit = MAX_WALKED_ENTRIES
                ));
            }
            let local = self.host.join(&local_parent, &name);
            let remote = remote_join(&remote_parent_directory, &name);
            if entry_is_directory {
                self.run_curl(
                    None,
                    vec![
                        "-Q".to_string(),
                        format!("MKD {remote}"),
                        curl_url(&self.target, &remote_parent_directory, true),
                    ],
                    cx,
                )
                .await
                // A directory that already exists is not a failure; whether
                // the tree is usable is decided by the file uploads below.
                .ok();
                for child in self.host.list_directory(local.clone(), cx).await? {
                    let child_is_directory = child.kind.is_directory();
                    pending.push((
                        local.clone(),
                        child.name,
                        remote.clone(),
                        child_is_directory,
                    ));
                }
            } else {
                self.run_curl(
                    Some(local_parent),
                    vec![
                        "-T".to_string(),
                        name,
                        curl_url(&self.target, &remote_parent_directory, true),
                    ],
                    cx,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn download_over_ftp(
        &self,
        remote_directory: &str,
        name: &str,
        is_directory: bool,
        local_directory: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let mut pending = vec![(
            remote_directory.to_string(),
            name.to_string(),
            local_directory.to_string(),
            is_directory,
        )];
        let mut walked = 0;
        while let Some((remote_parent_directory, name, local_parent, entry_is_directory)) =
            pending.pop()
        {
            walked += 1;
            if walked > MAX_WALKED_ENTRIES {
                bail!(i18n::t!(
                    "More than {limit} files would be transferred. \
                     Transfer them in smaller batches.",
                    limit = MAX_WALKED_ENTRIES
                ));
            }
            let remote = remote_join(&remote_parent_directory, &name);
            let local = self.host.join(&local_parent, &name);
            if entry_is_directory {
                self.host.make_directory(local.clone(), cx).await?;
                for entry in self.list_over_ftp(&remote, cx).await? {
                    let child_is_directory = entry.kind.is_directory();
                    pending.push((
                        remote.clone(),
                        entry.name,
                        local.clone(),
                        child_is_directory,
                    ));
                }
            } else {
                self.run_curl(
                    Some(local_parent),
                    vec![
                        "-o".to_string(),
                        name,
                        curl_url(&self.target, &remote, false),
                    ],
                    cx,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn run_sftp_script(
        &self,
        working_directory: Option<String>,
        script: String,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        self.host
            .run(
                HostCommand {
                    program: TransferProgram::Sftp,
                    args: sftp_args(&self.target),
                    stdin: Some(script),
                    working_directory,
                    wants_askpass: true,
                },
                cx,
            )
            .await
    }

    async fn run_scp(
        &self,
        working_directory: Option<String>,
        direction: TransferDirection,
        recursive: bool,
        local_argument: &str,
        remote_path: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        self.host
            .run(
                HostCommand {
                    program: TransferProgram::Scp,
                    args: scp_args(
                        &self.target,
                        direction,
                        recursive,
                        local_argument,
                        remote_path,
                    ),
                    stdin: None,
                    working_directory,
                    wants_askpass: true,
                },
                cx,
            )
            .await
            .map(|_| ())
    }

    async fn run_curl(
        &self,
        working_directory: Option<String>,
        args: Vec<String>,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let configuration = curl_config(&self.target, self.password.as_deref());
        self.host
            .run(
                HostCommand {
                    program: TransferProgram::Curl,
                    args: curl_args(&args, configuration.is_some()),
                    // Always piped, so that curl reading `--config -` sees the
                    // end of the file rather than waiting on a terminal.
                    stdin: Some(configuration.unwrap_or_default()),
                    working_directory,
                    wants_askpass: false,
                },
                cx,
            )
            .await
    }
}

/// The fixed part of every curl invocation. `--disable` has to come first for
/// curl to skip `.curlrc`, and the credential switch comes last so that the
/// operation's own `-o` / `-T` still sit next to their URL.
pub fn curl_args(operation: &[String], uses_config: bool) -> Vec<String> {
    let mut args = vec![
        "--disable".to_string(),
        "--silent".to_string(),
        "--show-error".to_string(),
        "--connect-timeout".to_string(),
        CONNECT_TIMEOUT_SECONDS.to_string(),
    ];
    args.extend_from_slice(operation);
    if uses_config {
        args.push("--config".to_string());
        args.push("-".to_string());
    } else {
        args.push("--netrc-optional".to_string());
    }
    args
}

/// Credentials go through a config file on stdin rather than `-u`, so the
/// password never appears in the process list. With no user name, curl is told
/// it may use `~/.netrc` instead (ADR-F28).
pub fn curl_config(target: &RemoteTarget, password: Option<&str>) -> Option<String> {
    let user = target
        .user
        .as_deref()
        .map(str::trim)
        .filter(|user| !user.is_empty())?;
    Some(format!(
        "user = \"{}\"\n",
        escape_curl_config(&format!("{user}:{}", password.unwrap_or_default()))
    ))
}

fn command_failure_message(stderr: &str, stdout: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        i18n::t!("The command failed (it printed no details).").to_string()
    } else {
        i18n::t!("The command failed:\n{detail}")
    }
}

/// Escapes a value for curl's `--config` syntax, where only `\` and `"` are
/// special inside a quoted value.
fn escape_curl_config(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Windows ships curl in System32; elsewhere it comes off `PATH`.
fn curl_program() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let bundled = PathBuf::from(system_root).join("System32").join("curl.exe");
        if bundled.is_file() {
            return bundled;
        }
    }
    PathBuf::from("curl")
}

// -------------------------------------------------------------------------
// This machine's file system
// -------------------------------------------------------------------------

/// Backs [`IdeExecHost::list_directory`], so the modal only ever deals in
/// [`DirectoryEntry`] regardless of which pane — or which machine — it is
/// talking about.
async fn list_local_directory(fs: &Arc<dyn Fs>, directory: &Path) -> Result<Vec<DirectoryEntry>> {
    let mut stream = fs.read_dir(directory).await?;
    let mut entries = Vec::new();
    while let Some(child) = stream.next().await {
        let child = child?;
        let Some(name) = child
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            continue;
        };
        let metadata = fs.metadata(&child).await.ok().flatten();
        let kind = match &metadata {
            Some(metadata) if metadata.is_dir => EntryKind::Directory,
            Some(metadata) if metadata.is_symlink => EntryKind::Symlink,
            _ => EntryKind::File,
        };
        entries.push(DirectoryEntry {
            name,
            kind,
            size: metadata
                .as_ref()
                .filter(|metadata| !metadata.is_dir)
                .map(|metadata| metadata.len),
            modified: metadata.as_ref().map(|metadata| {
                let timestamp: chrono::DateTime<chrono::Local> =
                    metadata.mtime.timestamp_for_user().into();
                timestamp.format("%Y-%m-%d %H:%M").to_string()
            }),
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// A human-readable size for the pane, with directories left blank.
pub fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return String::new();
    };
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    match UNITS.get(unit) {
        Some(&"B") | None => format!("{size} B"),
        Some(unit) => format!("{value:.1} {unit}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use gpui::TestAppContext;
    use util::path;

    use super::*;

    fn directory(name: &str) -> DirectoryEntry {
        DirectoryEntry {
            name: name.to_string(),
            kind: EntryKind::Directory,
            size: None,
            modified: None,
        }
    }

    #[test]
    fn parses_sftp_long_listing_in_both_date_formats() {
        let listing = "\
sftp> ls -l -a \"/home/user\"
drwxr-xr-x    5 user     group        4096 Sep  6 12:34 .
drwxr-xr-x    3 root     root         4096 Jan 10  2024 ..
drwxr-xr-x    2 user     group        4096 Sep  6 12:34 documents
-rw-r--r--    1 user     group      123456 Jan 10  2024 archive.tar.gz
-rw-r--r--    1 user     group          12 Sep  6 09:00 my notes.txt
lrwxrwxrwx    1 user     group          11 Sep  6 12:34 latest -> archive.tar.gz
";
        let entries = parse_sftp_listing(listing);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "documents");
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[0].modified.as_deref(), Some("Sep 6 12:34"));

        assert_eq!(entries[1].name, "archive.tar.gz");
        assert_eq!(entries[1].kind, EntryKind::File);
        assert_eq!(entries[1].size, Some(123456));
        assert_eq!(entries[1].modified.as_deref(), Some("Jan 10 2024"));

        assert_eq!(entries[2].name, "my notes.txt");
        assert_eq!(entries[2].size, Some(12));

        assert_eq!(entries[3].name, "latest");
        assert_eq!(entries[3].kind, EntryKind::Symlink);
    }

    #[test]
    fn parses_win32_openssh_listing_with_directory_prefixed_names() {
        // Windows の OpenSSH (System32\OpenSSH\sftp.exe) の実物: 権限が `******`、
        // リンク数が `?`、名前に一覧対象ディレクトリが前置される。
        let listing = "sftp> ls -l -a \"/home\"
drwx******    ? root     root         4096 Mar  4  2026 /home/.
drwx******    ? root     root         4096 Mar  4  2026 /home/..
drwx******    ? ubuntu   ubuntu       4096 Sep  5 19:39 /home/ubuntu
lrwx******    ? root     root            7 Apr 22  2024 /home/link -> /usr/bin
-rw-******    ? ubuntu   ubuntu         12 Sep  6 09:00 /home/my notes.txt
";
        let entries = parse_sftp_listing(listing);
        let names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["ubuntu", "link", "my notes.txt"]);
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[1].kind, EntryKind::Symlink);
        assert_eq!(entries[2].size, Some(12));

        let root = parse_sftp_listing(
            "drwx******    ? root     root         4096 Mar  4  2026 /.
drwx******    ? root     root         4096 Mar  4  2026 /..
drwx******    ? root     root         4096 Mar  4  2026 /home
",
        );
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "home");
    }

    #[test]
    fn normalize_remote_dir_collapses_repeated_slashes() {
        assert_eq!(normalize_remote_dir("//home"), "/home");
        assert_eq!(normalize_remote_dir("//home///ubuntu/"), "/home/ubuntu");
        assert_eq!(normalize_remote_dir("//"), "/");
        assert_eq!(remote_join("//home", "ubuntu"), "/home/ubuntu");
    }

    #[test]
    fn parses_japanese_names_from_sftp_listing() {
        let listing = "\
drwxr-xr-x    2 user     group        4096 Sep  6 12:34 資料 一式
-rw-r--r--    1 user     group         100 Sep  6 12:34 議事録.txt
";
        let entries = parse_sftp_listing(listing);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "資料 一式");
        assert_eq!(entries[1].name, "議事録.txt");
    }

    #[test]
    fn ignores_unparsable_sftp_lines() {
        let listing = "\
Connected to example.com.
Couldn't stat remote file: No such file or directory
-rw-r--r--    1 user     group         100 Sep  6 12:34 real.txt
";
        let entries = parse_sftp_listing(listing);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "real.txt");
    }

    #[test]
    fn parses_mlsd_listing() {
        let listing = "\
type=cdir;modify=20240110123456; .
type=pdir;modify=20240110123456; ..
type=dir;modify=20240110123456;size=4096;perm=el; 資料
type=file;size=1234;modify=20240110123456;perm=adfrw; notes and things.txt
";
        let entries = parse_mlsd_listing(listing);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "資料");
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[1].name, "notes and things.txt");
        assert_eq!(entries[1].size, Some(1234));
        assert_eq!(entries[1].modified.as_deref(), Some("2024-01-10 12:34"));
    }

    #[test]
    fn parses_ftp_list_in_unix_and_dos_formats() {
        let unix = "\
drwxr-xr-x    2 ftp      ftp          4096 Sep  6 12:34 pub
-rw-r--r--    1 ftp      ftp        123456 Sep  6 12:34 readme.txt
";
        let entries = parse_ftp_listing(unix);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "pub");
        assert_eq!(entries[0].kind, EntryKind::Directory);

        let dos = "\
01-10-24  12:34PM       <DIR>          documents
01-10-24  12:34PM                 1234 notes.txt
01-10-24  12:34PM                 4096 名前に 空白.txt
";
        let entries = parse_ftp_listing(dos);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "documents");
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[1].name, "notes.txt");
        assert_eq!(entries[1].size, Some(1234));
        assert_eq!(entries[1].modified.as_deref(), Some("01-10-24 12:34PM"));
        assert_eq!(entries[2].name, "名前に 空白.txt");
    }

    #[test]
    fn sorts_directories_before_files() {
        let mut entries = vec![
            DirectoryEntry {
                name: "beta.txt".into(),
                kind: EntryKind::File,
                size: None,
                modified: None,
            },
            directory("Zeta"),
            directory("alpha"),
        ];
        sort_entries(&mut entries);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "Zeta", "beta.txt"]
        );
    }

    fn ssh_target() -> RemoteTarget {
        RemoteTarget {
            protocol: TransferProtocol::Scp,
            host: "example.com".into(),
            port: 2222,
            user: Some("operator".into()),
        }
    }

    #[test]
    fn builds_sftp_arguments_that_allow_password_prompts() {
        let args = sftp_args(&RemoteTarget {
            protocol: TransferProtocol::Sftp,
            ..ssh_target()
        });
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=no"]));
        assert!(args.windows(2).any(|pair| pair == ["-P", "2222"]));
        assert!(args.windows(2).any(|pair| pair == ["-b", "-"]));
        assert_eq!(
            args.last().map(String::as_str),
            Some("operator@example.com")
        );
    }

    #[test]
    fn builds_recursive_scp_upload_with_port_and_quoting() {
        let args = scp_args(
            &ssh_target(),
            TransferDirection::Upload,
            true,
            "project",
            "/srv/my data",
        );
        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                "ConnectTimeout=15".to_string(),
                "-P".to_string(),
                "2222".to_string(),
                "-r".to_string(),
                "project".to_string(),
                "operator@example.com:'/srv/my data/'".to_string(),
            ]
        );
    }

    #[test]
    fn builds_scp_download_and_leaves_plain_paths_unquoted() {
        let args = scp_args(
            &ssh_target(),
            TransferDirection::Download,
            false,
            ".",
            "/srv/notes.txt",
        );
        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                "ConnectTimeout=15".to_string(),
                "-P".to_string(),
                "2222".to_string(),
                "operator@example.com:/srv/notes.txt".to_string(),
                ".".to_string(),
            ]
        );
    }

    #[test]
    fn quotes_only_remote_paths_that_need_it() {
        assert_eq!(scp_quote_remote("/srv/data.txt"), "/srv/data.txt");
        assert_eq!(scp_quote_remote("/srv/a b.txt"), "'/srv/a b.txt'");
        assert_eq!(scp_quote_remote("/srv/$HOME"), "'/srv/$HOME'");
        assert_eq!(scp_quote_remote("/srv/資料.txt"), "/srv/資料.txt");
    }

    fn ftp_target() -> RemoteTarget {
        RemoteTarget {
            protocol: TransferProtocol::Ftp,
            host: "files.example.com".into(),
            port: 2121,
            user: Some("anonymous".into()),
        }
    }

    #[test]
    fn escapes_curl_urls() {
        assert_eq!(
            curl_url(&ftp_target(), "/pub/my data", true),
            "ftp://files.example.com:2121/pub/my%20data/"
        );
        assert_eq!(
            curl_url(&ftp_target(), "/pub/notes#1.txt", false),
            "ftp://files.example.com:2121/pub/notes%231.txt"
        );
        assert_eq!(
            curl_url(&ftp_target(), "/資料", false),
            "ftp://files.example.com:2121/%E8%B3%87%E6%96%99"
        );
    }

    #[test]
    fn uses_the_ftps_scheme_for_tls() {
        let target = RemoteTarget {
            protocol: TransferProtocol::Ftps,
            ..ftp_target()
        };
        assert!(curl_url(&target, "/pub", true).starts_with("ftps://"));
    }

    #[test]
    fn escapes_curl_configuration_values() {
        assert_eq!(
            escape_curl_config("user:pa\"ss\\word"),
            "user:pa\\\"ss\\\\word"
        );
    }

    #[test]
    fn joins_and_walks_remote_paths() {
        assert_eq!(remote_join("/", "a"), "/a");
        assert_eq!(remote_join("/a/b/", "c"), "/a/b/c");
        assert_eq!(remote_parent("/a/b/c"), "/a/b");
        assert_eq!(remote_parent("/a"), "/");
        assert_eq!(remote_parent("/"), "/");
        assert_eq!(normalize_remote_dir("  a/b/  "), "/a/b");
        assert_eq!(normalize_remote_dir(""), "/");
    }

    #[test]
    fn rejects_paths_that_would_break_quoting() {
        assert!(validate_remote_path("/srv/data").is_ok());
        assert!(validate_remote_path("srv/data").is_err());
        assert!(validate_remote_path("/srv/\"data\"").is_err());
        assert!(validate_remote_path("/srv/a\\b").is_err());
        assert!(validate_entry_name("../etc").is_err());
        assert!(validate_entry_name("資料.txt").is_ok());
        assert!(validate_host("192.168.0.1").is_ok());
        assert!(validate_host("host name").is_err());
        assert!(validate_user("ops-1").is_ok());
        assert!(validate_user("ops 1").is_err());
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(None), "");
        assert_eq!(format_size(Some(512)), "512 B");
        assert_eq!(format_size(Some(2048)), "2.0 KB");
    }

    #[test]
    fn parses_linux_openssh_listing() {
        // OpenBSD/Linux の sftp(1): 名前だけ (ディレクトリ前置なし)、権限は実値。
        let listing = "sftp> ls -l -a \"/home/ubuntu\"
drwxr-xr-x    5 ubuntu   ubuntu       4096 Sep  6 12:34 .
drwxr-xr-x    3 root     root         4096 Mar  4  2026 ..
drwxrwxr-x    8 ubuntu   ubuntu       4096 Sep  5 19:39 zed-jp-llm
-rw-rw-r--    1 ubuntu   ubuntu    12345678 Sep  6 09:00 backup.tar.gz
lrwxrwxrwx    1 ubuntu   ubuntu          7 Apr 22  2024 bin -> /usr/bin
-rw-------    1 ubuntu   ubuntu        512 Jan 10  2024 .bash_history
";
        let entries = parse_sftp_listing(listing);
        let names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["zed-jp-llm", "backup.tar.gz", "bin", ".bash_history"]
        );
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[1].size, Some(12345678));
        assert_eq!(entries[1].modified.as_deref(), Some("Sep 6 09:00"));
        assert_eq!(entries[2].kind, EntryKind::Symlink);
    }

    #[test]
    fn curl_arguments_keep_the_credential_switch_last() {
        let operation = vec!["-T".to_string(), "notes.txt".to_string()];
        assert_eq!(
            curl_args(&operation, true),
            vec![
                "--disable",
                "--silent",
                "--show-error",
                "--connect-timeout",
                "15",
                "-T",
                "notes.txt",
                "--config",
                "-",
            ]
        );
        assert_eq!(
            curl_args(&operation, false).last().map(String::as_str),
            Some("--netrc-optional")
        );
    }

    #[test]
    fn curl_configuration_falls_back_to_netrc_without_a_user() {
        assert_eq!(
            curl_config(&ftp_target(), Some("pa\"ss")).as_deref(),
            Some("user = \"anonymous:pa\\\"ss\"\n")
        );
        assert_eq!(
            curl_config(
                &RemoteTarget {
                    user: None,
                    ..ftp_target()
                },
                Some("ignored")
            ),
            None
        );
    }

    #[test]
    fn the_stored_password_answers_once_then_hands_over_to_the_user() {
        let password = "ubuntu@192.168.100.150's password: ";
        assert_eq!(
            decide_stored_password_use(password, false),
            StoredPasswordUse::Answer
        );
        // The second prompt from the same `ssh` means it was refused; the user
        // types the next one rather than the screen spending another attempt.
        assert_eq!(
            decide_stored_password_use(password, true),
            StoredPasswordUse::Rejected
        );

        // A host key must never be confirmed on the user's behalf, and a key
        // passphrase is not the account password.
        for prompt in [
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "Enter passphrase for key '/home/ubuntu/.ssh/id_ed25519': ",
            "Verification code: ",
        ] {
            assert_eq!(
                decide_stored_password_use(prompt, false),
                StoredPasswordUse::AskUser,
                "{prompt:?}"
            );
            assert_eq!(
                decide_stored_password_use(prompt, true),
                StoredPasswordUse::AskUser,
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn host_key_questions_get_buttons_and_secrets_get_a_masked_field() {
        for prompt in [
            "The authenticity of host 'h (1.2.3.4)' can't be established.\n\
             ED25519 key fingerprint is SHA256:abcdef.\n\
             Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "Please type 'yes', 'no' or the fingerprint: ",
        ] {
            assert_eq!(
                askpass_answer_mode(prompt),
                AskPassAnswerMode::Confirm,
                "{prompt:?}"
            );
        }
        for prompt in [
            "ubuntu@192.168.100.150's password: ",
            "Enter passphrase for key '/home/ubuntu/.ssh/id_ed25519': ",
            // Not recognized, and more likely a one-time code than a name.
            "Verification code: ",
        ] {
            assert_eq!(
                askpass_answer_mode(prompt),
                AskPassAnswerMode::Secret,
                "{prompt:?}"
            );
        }
        for prompt in ["Username: ", "login as: "] {
            assert_eq!(
                askpass_answer_mode(prompt),
                AskPassAnswerMode::PlainText,
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn protocol_names_round_trip() {
        for protocol in TransferProtocol::ALL {
            assert_eq!(TransferProtocol::from_name(protocol.name()), Some(protocol));
        }
    }

    #[test]
    fn credential_keys_follow_the_quick_connect_pairing() {
        assert_eq!(
            credential_lookups(TransferProtocol::Sftp, 2222),
            [(ConnectionProtocol::Ssh, 2222)]
        );
        assert_eq!(
            credential_lookups(TransferProtocol::Scp, 22),
            [(ConnectionProtocol::Ssh, 22)]
        );
        // FTP's port is not the Telnet entry's, so the device's own port is
        // tried after the exact one.
        assert_eq!(
            credential_lookups(TransferProtocol::Ftp, 21),
            [
                (ConnectionProtocol::Telnet, 21),
                (ConnectionProtocol::Telnet, 23)
            ]
        );
        assert_eq!(
            credential_lookups(TransferProtocol::Ftps, 23),
            [(ConnectionProtocol::Telnet, 23)]
        );
    }

    #[test]
    fn reads_protocol_names_case_insensitively() {
        assert_eq!(
            TransferProtocol::from_name(" SFTP "),
            Some(TransferProtocol::Sftp)
        );
        assert_eq!(
            TransferProtocol::from_name("ftps"),
            Some(TransferProtocol::Ftps)
        );
        assert_eq!(TransferProtocol::from_name("http"), None);
    }

    // ---------------------------------------------------------------------
    // The execution host
    // ---------------------------------------------------------------------

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedCommand {
        program: &'static str,
        args: Vec<String>,
        stdin: Option<String>,
        working_directory: Option<String>,
        wants_askpass: bool,
    }

    /// An [`ExecHost`] that runs nothing: it records what it was asked to run
    /// and answers listings from a fixed table, so the engine's behaviour on
    /// either machine can be checked without a server or a `sftp` binary.
    struct FakeExecHost {
        remote: bool,
        stdout: String,
        listings: HashMap<String, Vec<DirectoryEntry>>,
        commands: Mutex<Vec<RecordedCommand>>,
        made_directories: Mutex<Vec<String>>,
    }

    impl FakeExecHost {
        fn new(remote: bool) -> Self {
            Self {
                remote,
                stdout: String::new(),
                listings: HashMap::new(),
                commands: Mutex::new(Vec::new()),
                made_directories: Mutex::new(Vec::new()),
            }
        }

        fn with_stdout(mut self, stdout: &str) -> Self {
            self.stdout = stdout.to_string();
            self
        }

        fn recorded(&self) -> Vec<RecordedCommand> {
            std::mem::take(&mut self.commands.lock().expect("the fake host is not poisoned"))
        }
    }

    impl ExecHost for FakeExecHost {
        fn label(&self) -> SharedString {
            "テスト".into()
        }

        fn is_remote(&self) -> bool {
            self.remote
        }

        fn root_directory(&self) -> String {
            "/".to_string()
        }

        fn join(&self, directory: &str, name: &str) -> String {
            if self.remote {
                remote_join(directory, name)
            } else {
                Path::new(directory)
                    .join(name)
                    .to_string_lossy()
                    .into_owned()
            }
        }

        fn parent(&self, directory: &str) -> Option<String> {
            if self.remote {
                let directory = normalize_remote_dir(directory);
                (directory != "/").then(|| remote_parent(&directory))
            } else {
                Path::new(directory)
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned())
            }
        }

        fn list_directory(
            &self,
            directory: String,
            _cx: &mut AsyncApp,
        ) -> Task<Result<Vec<DirectoryEntry>>> {
            Task::ready(Ok(self
                .listings
                .get(&directory)
                .cloned()
                .unwrap_or_default()))
        }

        fn make_directory(&self, path: String, _cx: &mut AsyncApp) -> Task<Result<()>> {
            self.made_directories
                .lock()
                .expect("the fake host is not poisoned")
                .push(path);
            Task::ready(Ok(()))
        }

        fn rename(&self, _from: String, _to: String, _cx: &mut AsyncApp) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn remove(&self, _path: String, _kind: EntryKind, _cx: &mut AsyncApp) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn run(&self, command: HostCommand, _cx: &mut AsyncApp) -> Task<Result<String>> {
            self.commands
                .lock()
                .expect("the fake host is not poisoned")
                .push(RecordedCommand {
                    program: command.program.name(),
                    args: command.args,
                    stdin: command.stdin,
                    working_directory: command.working_directory,
                    wants_askpass: command.wants_askpass,
                });
            Task::ready(Ok(self.stdout.clone()))
        }
    }

    fn sftp_target() -> RemoteTarget {
        RemoteTarget {
            protocol: TransferProtocol::Sftp,
            ..ssh_target()
        }
    }

    #[gpui::test]
    async fn lists_through_the_execution_host(cx: &mut TestAppContext) {
        let host = Arc::new(FakeExecHost::new(true).with_stdout(
            "sftp> ls -l -a \"/srv\"
drwxr-xr-x    2 ubuntu   ubuntu       4096 Sep  6 12:34 incoming
-rw-r--r--    1 ubuntu   ubuntu        120 Sep  6 12:34 readme.txt
",
        ));
        let backend = TransferBackend::new(sftp_target(), host.clone(), None);
        let mut cx = cx.to_async();

        let entries = backend
            .list("/srv", &mut cx)
            .await
            .expect("the fake host always answers");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["incoming", "readme.txt"]
        );

        let commands = host.recorded();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "sftp");
        assert_eq!(commands[0].stdin.as_deref(), Some("ls -l -a \"/srv\"\n"));
        // The listing needs the same authentication as a transfer does.
        assert!(commands[0].wants_askpass);
    }

    #[gpui::test]
    async fn scp_uses_absolute_paths_on_the_remote_host(cx: &mut TestAppContext) {
        let host = Arc::new(FakeExecHost::new(true));
        let backend = TransferBackend::new(ssh_target(), host.clone(), None);
        let mut cx = cx.to_async();

        backend
            .upload("/home/ubuntu/work", "notes.txt", false, "/srv", &mut cx)
            .await
            .expect("the fake host always answers");
        backend
            .download("/srv", "notes.txt", false, "/home/ubuntu/work", &mut cx)
            .await
            .expect("the fake host always answers");

        let commands = host.recorded();
        assert_eq!(commands.len(), 2);
        // No working directory is needed: a `/`-rooted path has a slash before
        // any colon, so `scp` cannot mistake it for `host:path`.
        assert_eq!(commands[0].working_directory, None);
        assert_eq!(
            commands[0].args.iter().rev().take(2).collect::<Vec<_>>(),
            vec!["operator@example.com:/srv/", "/home/ubuntu/work/notes.txt"]
        );
        assert_eq!(commands[1].working_directory, None);
        assert_eq!(
            commands[1].args.last().map(String::as_str),
            Some("/home/ubuntu/work")
        );
    }

    #[gpui::test]
    async fn scp_keeps_the_working_directory_trick_on_this_machine(cx: &mut TestAppContext) {
        let host = Arc::new(FakeExecHost::new(false));
        let backend = TransferBackend::new(ssh_target(), host.clone(), None);
        let mut cx = cx.to_async();

        backend
            .upload(path!("/work"), "notes.txt", false, "/srv", &mut cx)
            .await
            .expect("the fake host always answers");
        backend
            .download("/srv", "notes.txt", false, path!("/work"), &mut cx)
            .await
            .expect("the fake host always answers");

        let commands = host.recorded();
        assert_eq!(
            commands[0].working_directory.as_deref(),
            Some(path!("/work"))
        );
        assert_eq!(
            commands[0].args.iter().rev().take(2).collect::<Vec<_>>(),
            vec!["operator@example.com:/srv/", "notes.txt"]
        );
        assert_eq!(
            commands[1].working_directory.as_deref(),
            Some(path!("/work"))
        );
        assert_eq!(commands[1].args.last().map(String::as_str), Some("."));
    }

    #[gpui::test]
    async fn ftp_download_creates_directories_through_the_host(cx: &mut TestAppContext) {
        let host = Arc::new(FakeExecHost::new(true));
        let backend = TransferBackend::new(ftp_target(), host.clone(), Some("secret".into()));
        let mut cx = cx.to_async();

        // The empty `MLSD` answer ends the walk after the directory itself.
        backend
            .download("/pub", "data", true, "/home/ubuntu", &mut cx)
            .await
            .expect("the fake host always answers");

        assert_eq!(
            host.made_directories
                .lock()
                .expect("the fake host is not poisoned")
                .as_slice(),
            ["/home/ubuntu/data"]
        );
        let commands = host.recorded();
        assert!(commands.iter().all(|command| command.program == "curl"));
        // The password reaches curl through stdin, never the command line.
        assert!(
            commands
                .iter()
                .all(|command| !command.args.iter().any(|arg| arg.contains("secret")))
        );
        assert_eq!(
            commands[0].stdin.as_deref(),
            Some("user = \"anonymous:secret\"\n")
        );
    }

    #[gpui::test]
    async fn ftp_upload_walks_the_host_it_is_reading_from(cx: &mut TestAppContext) {
        let mut host = FakeExecHost::new(true);
        host.listings.insert(
            "/home/ubuntu/data".to_string(),
            vec![DirectoryEntry {
                name: "notes.txt".into(),
                kind: EntryKind::File,
                size: Some(12),
                modified: None,
            }],
        );
        let host = Arc::new(host);
        let backend = TransferBackend::new(ftp_target(), host.clone(), None);
        let mut cx = cx.to_async();

        backend
            .upload("/home/ubuntu", "data", true, "/pub", &mut cx)
            .await
            .expect("the fake host always answers");

        let commands = host.recorded();
        assert_eq!(commands.len(), 2);
        assert!(
            commands[0]
                .args
                .iter()
                .any(|argument| argument == "MKD /pub/data")
        );
        // The file is uploaded from the directory holding it, so only its bare
        // name has to survive the command line.
        assert_eq!(
            commands[1].working_directory.as_deref(),
            Some("/home/ubuntu/data")
        );
        assert!(
            commands[1]
                .args
                .windows(2)
                .any(|pair| pair == ["-T", "notes.txt"])
        );
        assert!(
            commands[1]
                .args
                .iter()
                .any(|argument| argument == "ftp://files.example.com:2121/pub/data/")
        );
    }
}
