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
//! pure functions, and only [`TransferBackend`] spawns processes.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use askpass::{AskPassDelegate, AskPassResult, AskPassSession};
use fs::{Fs, RemoveOptions, RenameOptions};
use futures::{AsyncWriteExt as _, FutureExt as _, StreamExt as _, select_biased};
use gpui::AsyncApp;

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
        return Err("ホスト名または IP アドレスを入力してください。");
    }
    if host.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':' | '[' | ']')
    }) {
        Ok(())
    } else {
        Err("ホスト名に使用できない文字が含まれています。")
    }
}

pub fn validate_user(user: &str) -> Result<(), &'static str> {
    if user.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '@')
    }) {
        Ok(())
    } else {
        Err("ユーザー名に使用できない文字が含まれています。")
    }
}

/// Remote paths are always absolute and `/`-separated. The rejected characters
/// are the ones that would have to be escaped inside an `sftp` batch argument
/// or inside an FTP protocol command, and no remote file system needs them.
pub fn validate_remote_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("リモートのパスを入力してください。");
    }
    if !path.starts_with('/') {
        return Err("リモートのパスは / から始めてください。");
    }
    if path
        .chars()
        .any(|character| matches!(character, '"' | '\\') || character.is_control())
    {
        return Err("リモートのパスに使用できない文字が含まれています。");
    }
    Ok(())
}

/// A single path component typed by the user (new folder, rename target).
pub fn validate_entry_name(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err("名前を入力してください。");
    }
    if name == "." || name == ".." {
        return Err("その名前は使用できません。");
    }
    if name.chars().any(|character| {
        matches!(
            character,
            '/' | '\\' | '"' | ':' | '*' | '?' | '<' | '>' | '|'
        ) || character.is_control()
    }) {
        return Err("名前に使用できない文字が含まれています。");
    }
    Ok(())
}

pub fn normalize_remote_dir(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let with_root = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
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

/// `scp` command line. The local side is always a bare file name, with the
/// process's working directory set to the local pane's directory: a Windows
/// path would otherwise have to survive `scp`'s `host:path` splitting on the
/// drive letter's colon.
pub fn scp_args(
    target: &RemoteTarget,
    direction: TransferDirection,
    recursive: bool,
    local_name: &str,
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
            args.push(local_name.to_string());
            args.push(format!(
                "{destination}:{}",
                scp_quote_remote(&format!("{}/", normalize_remote_dir(remote_path)))
            ));
        }
        TransferDirection::Download => {
            args.push(format!("{destination}:{}", scp_quote_remote(remote_path)));
            args.push(".".to_string());
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
// Running the commands
// -------------------------------------------------------------------------

/// Builds the askpass delegate that answers SSH's prompts in the GUI. It is a
/// factory rather than a delegate because each spawned process needs its own
/// session, and a session cannot outlive the process it belongs to.
pub type AskPassFactory = Arc<dyn Fn(&mut AsyncApp) -> AskPassDelegate + Send + Sync>;

/// Guard against a pathological tree turning a single click into an unbounded
/// number of processes.
const MAX_WALKED_ENTRIES: usize = 2000;

pub struct TransferBackend {
    target: RemoteTarget,
    fs: Arc<dyn Fs>,
    askpass: AskPassFactory,
    /// FTP only, held for the life of the connection and never written to
    /// disk (ADR-F28). `None` lets curl fall back to `.netrc`.
    password: Option<String>,
}

impl TransferBackend {
    pub fn new(
        target: RemoteTarget,
        fs: Arc<dyn Fs>,
        askpass: AskPassFactory,
        password: Option<String>,
    ) -> Self {
        Self {
            target,
            fs,
            askpass,
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
                .run_sftp_script(None, &format!("ls -l -a {}\n", sftp_quote(&directory)), cx)
                .await?;
            parse_sftp_listing(&stdout)
        } else {
            self.list_over_ftp(&directory).await?
        };
        sort_entries(&mut entries);
        Ok(entries)
    }

    async fn list_over_ftp(&self, directory: &str) -> Result<Vec<DirectoryEntry>> {
        let url = curl_url(&self.target, directory, true);
        let mlsd = self
            .run_curl(
                None,
                vec!["-X".to_string(), "MLSD".to_string(), url.clone()],
            )
            .await;
        match mlsd {
            Ok(stdout) if stdout.contains("type=") => Ok(parse_mlsd_listing(&stdout)),
            Ok(stdout) if stdout.trim().is_empty() => Ok(Vec::new()),
            // Servers without `MLSD` answer 500; fall back to `LIST`, which
            // every FTP server has.
            _ => {
                let stdout = self.run_curl(None, vec![url]).await?;
                Ok(parse_ftp_listing(&stdout))
            }
        }
    }

    pub async fn make_directory(&self, path: &str, cx: &mut AsyncApp) -> Result<()> {
        validate_remote_path(path).map_err(|message| anyhow!("{message}"))?;
        if self.target.protocol.is_ssh() {
            self.run_sftp_script(None, &format!("mkdir {}\n", sftp_quote(path)), cx)
                .await?;
        } else {
            self.run_curl(
                None,
                vec![
                    "-Q".to_string(),
                    format!("MKD {path}"),
                    curl_url(&self.target, &remote_parent(path), true),
                ],
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
            self.run_sftp_script(None, &script, cx).await?;
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
                    bail!(
                        "削除の対象が {MAX_WALKED_ENTRIES} 件を超えました。\
                         フォルダーを分けて削除してください。"
                    );
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
            self.run_sftp_script(None, &script, cx).await?;
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
            self.run_curl(None, args).await?;
        }
        Ok(())
    }

    /// Sends `local_directory/name` into `remote_directory`.
    pub async fn upload(
        &self,
        local_directory: &Path,
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
                self.run_sftp_script(Some(local_directory), &script, cx)
                    .await?;
                Ok(())
            }
            TransferProtocol::Scp => {
                self.run_scp(
                    local_directory,
                    TransferDirection::Upload,
                    is_directory,
                    name,
                    &remote_directory,
                    cx,
                )
                .await
            }
            TransferProtocol::Ftp | TransferProtocol::Ftps => {
                self.upload_over_ftp(local_directory, name, is_directory, &remote_directory)
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
        local_directory: &Path,
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
                self.run_sftp_script(Some(local_directory), &script, cx)
                    .await?;
                Ok(())
            }
            TransferProtocol::Scp => {
                self.run_scp(
                    local_directory,
                    TransferDirection::Download,
                    is_directory,
                    name,
                    &remote_path,
                    cx,
                )
                .await
            }
            TransferProtocol::Ftp | TransferProtocol::Ftps => {
                self.download_over_ftp(&remote_path, name, is_directory, local_directory)
                    .await
            }
        }
    }

    /// curl transfers one file per invocation, so a directory is walked here
    /// and turned into `MKD` plus one upload per file (ADR-F28).
    async fn upload_over_ftp(
        &self,
        local_directory: &Path,
        name: &str,
        is_directory: bool,
        remote_directory: &str,
    ) -> Result<()> {
        let mut pending = vec![(
            local_directory.join(name),
            remote_join(remote_directory, name),
            is_directory,
        )];
        let mut walked = 0;
        while let Some((local, remote, local_is_directory)) = pending.pop() {
            walked += 1;
            if walked > MAX_WALKED_ENTRIES {
                bail!(
                    "転送するファイルが {MAX_WALKED_ENTRIES} 件を超えました。\
                     フォルダーを分けて転送してください。"
                );
            }
            if local_is_directory {
                self.run_curl(
                    None,
                    vec![
                        "-Q".to_string(),
                        format!("MKD {remote}"),
                        curl_url(&self.target, &remote_parent(&remote), true),
                    ],
                )
                .await
                // A directory that already exists is not a failure; whether
                // the tree is usable is decided by the file uploads below.
                .ok();
                for child in self.read_local_directory(&local).await? {
                    let Some(child_name) = child
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                    else {
                        continue;
                    };
                    let child_is_directory = self.fs.is_dir(&child).await;
                    pending.push((child, remote_join(&remote, &child_name), child_is_directory));
                }
            } else {
                let parent = local.parent().unwrap_or(local_directory).to_path_buf();
                let file_name = local
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .context("アップロードするファイル名を特定できませんでした")?;
                self.run_curl(
                    Some(&parent),
                    vec![
                        "-T".to_string(),
                        file_name,
                        curl_url(&self.target, &remote_parent(&remote), true),
                    ],
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn download_over_ftp(
        &self,
        remote_path: &str,
        name: &str,
        is_directory: bool,
        local_directory: &Path,
    ) -> Result<()> {
        let mut pending = vec![(
            remote_path.to_string(),
            local_directory.join(name),
            is_directory,
        )];
        let mut walked = 0;
        while let Some((remote, local, remote_is_directory)) = pending.pop() {
            walked += 1;
            if walked > MAX_WALKED_ENTRIES {
                bail!(
                    "転送するファイルが {MAX_WALKED_ENTRIES} 件を超えました。\
                     フォルダーを分けて転送してください。"
                );
            }
            if remote_is_directory {
                self.fs.create_dir(&local).await?;
                for entry in self.list_over_ftp(&remote).await? {
                    pending.push((
                        remote_join(&remote, &entry.name),
                        local.join(&entry.name),
                        entry.kind.is_directory(),
                    ));
                }
            } else {
                let parent = local.parent().unwrap_or(local_directory).to_path_buf();
                let file_name = local
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .context("保存するファイル名を特定できませんでした")?;
                self.run_curl(
                    Some(&parent),
                    vec![
                        "-o".to_string(),
                        file_name,
                        curl_url(&self.target, &remote, false),
                    ],
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn read_local_directory(&self, directory: &Path) -> Result<Vec<PathBuf>> {
        let mut stream = self.fs.read_dir(directory).await?;
        let mut children = Vec::new();
        while let Some(child) = stream.next().await {
            children.push(child?);
        }
        Ok(children)
    }

    async fn run_sftp_script(
        &self,
        working_directory: Option<&Path>,
        script: &str,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let mut command = util::command::new_command("sftp");
        command
            .args(sftp_args(&self.target))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(working_directory) = working_directory {
            command.current_dir(working_directory);
        }
        self.run_with_askpass(command, Some(script.to_string()), cx)
            .await
    }

    async fn run_scp(
        &self,
        local_directory: &Path,
        direction: TransferDirection,
        recursive: bool,
        local_name: &str,
        remote_path: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let mut command = util::command::new_command("scp");
        command
            .args(scp_args(
                &self.target,
                direction,
                recursive,
                local_name,
                remote_path,
            ))
            .current_dir(local_directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        self.run_with_askpass(command, None, cx).await.map(|_| ())
    }

    /// Runs an OpenSSH command with Zed's askpass helper wired up, so
    /// passphrase, password and first-time host-key prompts appear in the GUI
    /// exactly as they do for a remote project (ADR-F28).
    async fn run_with_askpass(
        &self,
        mut command: util::command::Command,
        stdin: Option<String>,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let delegate = (self.askpass)(cx);
        let mut askpass = AskPassSession::new(cx.background_executor().clone(), delegate).await?;
        command
            .env("SSH_ASKPASS", askpass.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force");
        #[cfg(target_os = "windows")]
        command.env("ZED_ASKPASS_SOCKET", askpass.socket_path());

        let mut child = command.spawn()?;
        if let Some(stdin) = stdin {
            let mut handle = child
                .stdin
                .take()
                .context("sftp の標準入力を開けませんでした")?;
            handle.write_all(stdin.as_bytes()).await?;
            handle.flush().await?;
            drop(handle);
        }

        // No timeout is given to `run`: a transfer may legitimately go a long
        // time without prompting, and `ConnectTimeout` already bounds the
        // unreachable-host case.
        let output = select_biased! {
            result = askpass.run(None).fuse() => {
                match result {
                    AskPassResult::CancelledByUser => bail!("認証がキャンセルされました。"),
                    AskPassResult::Timedout => bail!("接続がタイムアウトしました。"),
                }
            }
            output = child.output().fuse() => output?,
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            bail!(command_failure_message(&stderr, &stdout));
        }
        Ok(stdout)
    }

    async fn run_curl(
        &self,
        working_directory: Option<&Path>,
        args: Vec<String>,
    ) -> Result<String> {
        let mut command = util::command::new_command(curl_program());
        // `--disable` has to come first for curl to skip `.curlrc`.
        command
            .arg("--disable")
            .args([
                "--silent",
                "--show-error",
                "--connect-timeout",
                &CONNECT_TIMEOUT_SECONDS.to_string(),
            ])
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(working_directory) = working_directory {
            command.current_dir(working_directory);
        }

        // Credentials go through a config file on stdin rather than `-u`, so
        // the password never appears in the process list. With no user name,
        // curl is told it may use `~/.netrc` instead (ADR-F28).
        let mut configuration = String::new();
        match self.target.user.as_deref().filter(|user| !user.is_empty()) {
            Some(user) => {
                command.arg("--config").arg("-");
                configuration.push_str(&format!(
                    "user = \"{}\"\n",
                    escape_curl_config(&format!(
                        "{user}:{}",
                        self.password.as_deref().unwrap_or_default()
                    ))
                ));
            }
            None => {
                command.arg("--netrc-optional");
            }
        }

        let mut child = command.spawn()?;
        if let Some(mut handle) = child.stdin.take() {
            handle.write_all(configuration.as_bytes()).await?;
            handle.flush().await?;
            drop(handle);
        }
        let output = child.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            bail!(command_failure_message(&stderr, &stdout));
        }
        Ok(stdout)
    }
}

fn command_failure_message(stderr: &str, stdout: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        "コマンドが失敗しました (詳細な出力はありません)。".to_string()
    } else {
        format!("コマンドが失敗しました:\n{detail}")
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
// Local side
// -------------------------------------------------------------------------

/// Local-side operations live here too, so the modal only ever deals in
/// [`DirectoryEntry`] regardless of which pane it is talking about.
pub async fn list_local_directory(
    fs: &Arc<dyn Fs>,
    directory: &Path,
) -> Result<Vec<DirectoryEntry>> {
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

pub async fn make_local_directory(fs: &Arc<dyn Fs>, path: &Path) -> Result<()> {
    fs.create_dir(path).await
}

pub async fn rename_local(fs: &Arc<dyn Fs>, from: &Path, to: &Path) -> Result<()> {
    fs.rename(from, to, RenameOptions::default()).await
}

pub async fn remove_local(fs: &Arc<dyn Fs>, path: &Path, kind: EntryKind) -> Result<()> {
    let options = RemoveOptions {
        recursive: true,
        ignore_if_not_exists: false,
    };
    if kind.is_directory() {
        fs.remove_dir(path, options).await
    } else {
        fs.remove_file(path, options).await
    }
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
            "notes.txt",
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
}
