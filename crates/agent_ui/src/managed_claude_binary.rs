//! ADR-F49 decision 2: the Linux build of Claude Code that runs in the WSL
//! sandbox, fetched through the egress allowlist with the user's consent.
//!
//! The source is Anthropic's release bucket, the one the native installer uses.
//! Trust rests on Anthropic's release signing key, embedded below as its RSA
//! public key: each version's manifest must carry a detached OpenPGP signature
//! by that key, name the version that was asked for, and list the SHA-256 and
//! size the download then has to match. npm was rejected because its integrity
//! data only proves what the registry received, not that Anthropic built it.
//!
//! The signature check reads only the one shape Anthropic publishes — a single
//! version 4 RSA signature over a binary document, with SHA-2 — rather than
//! bringing a general OpenPGP implementation into the build for it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use collections::HashMap;
use futures::{
    AsyncReadExt as _, FutureExt as _, StreamExt as _,
    channel::mpsc,
    future::{LocalBoxFuture, Shared, WeakShared},
    lock::Mutex as AsyncMutex,
};
use gpui::{
    App, AsyncWindowContext, BackgroundExecutor, Global, PromptButton, PromptLevel, SharedString,
    Task, Window,
};
use http_client::{
    AsyncBody, EgressPolicyHandle, HttpClient, Response, egress_denied_hosts,
    hosts_required_to_download,
};
use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey, traits::PublicKeyParts as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha384, Sha512};
use util::ResultExt as _;

use crate::agent_registry_ui::{
    add_consented_hosts, allowlist_write_error, await_allowlist_pickup, missing_hosts,
};

const RELEASES_URL: &str = "https://downloads.claude.ai/claude-code-releases";
/// The RSA modulus of "Anthropic Claude Code Release Signing
/// <security@anthropic.com>" (RSA-4096, published at
/// `https://downloads.claude.ai/keys/claude-code.asc`), taken once from the
/// copy in `managed_claude_binary/anthropic-claude-code-release.asc`. A test
/// keeps it, the exponent and the fingerprint in step with that file.
const RELEASE_SIGNING_KEY_MODULUS: &str = concat!(
    "p28rSV5I8HmK8CK9GixBUZR/gtJxeOCsRXO4EJiej40jzBmQA3cWXGosVO82ZfFsRKVTtMC5iB/H",
    "H9sxjncrmYNWGroJNbx29m/FgYQBgkCXT4AfFl6rnnXqRGLZOerj/4AqE4yQ1GZbhBgR55Z7ro0i",
    "eKK8RHYUspBKAFHyWRhCCz6THW6YRbf0p/hG/08TOY6Sj3cJ7/AEoTRf9ZmVNX1k0KvbUSiVGpGY",
    "9OIHWgxRJUF2pArU4o/hk+sqGAgEUh8Bjvjwvz6+quLXPg+y0Y8Ugb1Fg6BUppam/zydYY/Q/+yN",
    "jnuF154gD1jEeeir8R5czs6zUHSbo2yXUpAsIdWYo5End8vGsluVmFExnUWm/fTVMGoM5Wm3v1VR",
    "epMydEnJ+atz4oQdmPQcKNAip5GJO2uyk++xFr9CpKvlR5jral92toYV/m+mur3va8ydamWBo/qG",
    "7/wt0sdS81IwH6lcu0SQ39rgKD+bdoPLv05EqVMYTFRI2QZEsWGYTMs0DOrfCIJFH50qyD0x4sWw",
    "1gEWeG3jDgY8cj2StZz+zjqzUd05CibcCzEAGm1EQg5y9D40tIsAU1OI7bpgQ9V0lC8lrqE7zJY6",
    "6UK9Z1daA8jrdDi6migNjHFrXfT3V4QvMthCIO05q05SS3x2G3ZpIgmI+CePUPB1pDf+lhPkU1M=",
);
const RELEASE_SIGNING_KEY_EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];
const RELEASE_SIGNING_KEY_FINGERPRINT: &str = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE";
/// The CLI version the host ran when this was pinned (2026-09-14). Anything
/// newer only arrives through an explicit update.
const PINNED_VERSION: Version = Version {
    major: 2,
    minor: 1,
    patch: 270,
};
const BINARY_NAME: &str = "claude";
const COMPRESSED_BINARY_NAME: &str = "claude.zst";
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;
const MAX_PACKET_BYTES: usize = 16 * 1024;
const MAX_POINTER_BYTES: u64 = 64;
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
const STALE_DOWNLOAD_AGE: Duration = Duration::from_secs(60 * 60);
/// How long a download may go without receiving anything before it is given up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// The architecture, the CPU flags (x64 only) and which C library the distro
/// runs on. Every answer starts with `PROBE_MARKER`, so that whatever else WSL
/// writes to the same stream cannot be mistaken for one.
#[cfg(target_os = "windows")]
const PROBE_SCRIPT: &str = r#"
printf 'zed-probe machine %s\n' "$(uname -m)"
printf 'zed-probe cpu-flags %s\n' "$(grep -m 1 '^flags' /proc/cpuinfo | cut -d: -f2-)"
if getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
    printf 'zed-probe libc glibc\n'
elif ldd --version 2>&1 | grep -qi musl || ls /lib/ld-musl-* >/dev/null 2>&1; then
    printf 'zed-probe libc musl\n'
else
    printf 'zed-probe libc unknown\n'
fi
"#;
const PROBE_MARKER: &str = "zed-probe ";

const SIGNATURE_PACKET_TAG: u8 = 2;
const SIGNATURE_VERSION: u8 = 4;
const BINARY_DOCUMENT_SIGNATURE: u8 = 0x00;
const RSA_ALGORITHM: u8 = 1;
const SUBPACKET_CREATION_TIME: u8 = 2;
const SUBPACKET_EXPIRATION_TIME: u8 = 3;
const SUBPACKET_ISSUER_KEY_ID: u8 = 16;
const SUBPACKET_ISSUER_FINGERPRINT: u8 = 33;
/// The fixed part of a version 4 signature's hash trailer (RFC 4880 §5.2.4).
const SIGNATURE_TRAILER_PREFIX: [u8; 2] = [0x04, 0xff];

const CANCEL_LABEL: &str = "追加せず中止";
const DOWNLOAD_DECLINED: &str = "Linux 版 Claude Code の取得を中止しました。";
const UPDATE_CHECK_DECLINED: &str = "Claude Code の更新の確認を中止しました。";
const SIGNATURE_REFUSED: &str = "Claude Code のリリース情報の署名を検証できませんでした。改ざんされている可能性があるため中止しました。";
const CHECKSUM_REFUSED: &str = "ダウンロードした Claude Code のハッシュがリリース情報と一致しません。改ざんされている可能性があるため中止しました。";
const SIZE_REFUSED: &str =
    "ダウンロードした Claude Code の大きさがリリース情報と一致しません。中止しました。";
const NO_AVX2: &str =
    "WSL の CPU で AVX2 を確認できないため、Linux 版 Claude Code (x64) は動作しません。";
const NOT_GLIBC: &str = "この WSL ディストリビューションは musl のため、Linux 版 Claude Code (glibc 版) は動作しません。";
const PROBE_UNREADABLE: &str = "WSL のアーキテクチャを調べられませんでした。";
const STALLED: &str = "Claude Code の配布元から 60 秒間応答がないため中止しました。";
const REDIRECT_REFUSED: &str = "Claude Code の配布元が別の場所へ転送しようとしたため中止しました。";
const SAVE_FAILED: &str =
    "ダウンロードした Claude Code を保存できませんでした。ディスクの空き容量を確認してください。";
const EXTRACT_FAILED: &str = "ダウンロードした Claude Code を展開できませんでした。";
const STAGING_FAILED: &str = "Claude Code を WSL に配置できませんでした。";
const STAGED_CHECK_FAILED: &str = "WSL に配置済みの Claude Code を確認できませんでした。";
#[cfg(not(target_os = "windows"))]
const WINDOWS_ONLY: &str = "サンドボックス用の Claude Code は Windows (WSL) でのみ使えます。";

/// A Claude Code binary staged inside WSL. Run it by this absolute path, never
/// by looking `claude` up on `PATH`: inside WSL `PATH` reaches the Windows
/// drives, where an npm shim would start a Windows program through interop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedClaudeBinary {
    pub version: String,
    pub sha256: String,
    pub path_in_wsl: String,
}

/// Which pointer in the release bucket an update follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeReleaseChannel {
    Stable,
    Latest,
}

impl ClaudeReleaseChannel {
    fn pointer_name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Latest => "latest",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Stable => "安定版",
            Self::Latest => "最新版",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedClaudeProgress {
    CheckingStaged,
    Verifying,
    Downloading { received: u64, total: u64 },
    Extracting,
    Staging,
}

impl ManagedClaudeProgress {
    pub fn message(&self) -> SharedString {
        match self {
            Self::CheckingStaged => "配置済みの Claude Code を確認しています…".into(),
            Self::Verifying => "Claude Code のリリース情報の署名を確認しています…".into(),
            Self::Downloading { received, total } => format!(
                "Claude Code をダウンロードしています… ({} / {} MB)",
                megabytes(*received),
                megabytes(*total)
            )
            .into(),
            Self::Extracting => "展開してハッシュを確認しています…".into(),
            Self::Staging => "WSL に配置しています…".into(),
        }
    }
}

fn megabytes(bytes: u64) -> u64 {
    (bytes + 512 * 1024) / (1024 * 1024)
}

/// The Claude Code binary for the sandbox in `distro` (`None`: the default
/// distro): the one already staged there when it is intact, otherwise the
/// installed version (the pinned one on first use) downloaded, verified and
/// staged. Asks before the first download of each version, and whenever the
/// release host is not on the network allowlist; declining makes no request.
///
/// A call for a distro that already has one running joins it (one download,
/// one question); progress is reported to the caller that started it.
pub fn ensure_managed_claude_binary(
    distro: Option<String>,
    window: &mut Window,
    cx: &mut App,
    on_progress: impl Fn(&ManagedClaudeProgress, &mut Window, &mut App) + 'static,
) -> Task<Result<ManagedClaudeBinary>> {
    join_or_start(distro.clone(), cx, move |cx| {
        run_installer(distro, Goal::Ensure, window, cx, on_progress)
    })
}

/// The user's "update" button: reads the version `channel` points at, shows it,
/// and installs it. Never moves to a version older than the installed one.
pub fn update_managed_claude_binary(
    distro: Option<String>,
    channel: ClaudeReleaseChannel,
    window: &mut Window,
    cx: &mut App,
    on_progress: impl Fn(&ManagedClaudeProgress, &mut Window, &mut App) + 'static,
) -> Task<Result<ManagedClaudeBinary>> {
    run_installer(distro, Goal::Update(channel), window, cx, on_progress)
}

type InstallTask = Task<Result<ManagedClaudeBinary, Arc<anyhow::Error>>>;

#[derive(Default)]
struct InFlightInstalls {
    /// The `ensure` running for each distro, held weakly: it lives only while
    /// some caller still awaits it, so dropping every task it handed out
    /// cancels it. A finished one is replaced, never joined.
    installs: HashMap<Option<String>, WeakShared<InstallTask>>,
    /// Serializes every installer's read-modify-write of the state file.
    state_lock: Arc<AsyncMutex<()>>,
}

impl Global for InFlightInstalls {}

fn join_or_start(
    distro: Option<String>,
    cx: &mut App,
    start: impl FnOnce(&mut App) -> Task<Result<ManagedClaudeBinary>>,
) -> Task<Result<ManagedClaudeBinary>> {
    let running = cx
        .default_global::<InFlightInstalls>()
        .installs
        .get(&distro)
        .and_then(WeakShared::upgrade)
        .filter(|install| install.peek().is_none());
    let install = match running {
        Some(install) => install,
        None => {
            let task = start(cx);
            let install: Shared<InstallTask> = cx
                .spawn(async move |_| task.await.map_err(Arc::new))
                .shared();
            if let Some(weak) = install.downgrade() {
                cx.default_global::<InFlightInstalls>()
                    .installs
                    .insert(distro, weak);
            }
            install
        }
    };
    cx.spawn(async move |_| {
        install
            .await
            .map_err(|error| anyhow::Error::new(SharedFailure(error)))
    })
}

/// One install's failure as every caller that shared it receives it: the same
/// message, with the same chain behind it.
#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);

impl fmt::Display for SharedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.0, f)
    }
}

impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.chain().nth(1)
    }
}

fn run_installer(
    distro: Option<String>,
    goal: Goal,
    window: &mut Window,
    cx: &mut App,
    on_progress: impl Fn(&ManagedClaudeProgress, &mut Window, &mut App) + 'static,
) -> Task<Result<ManagedClaudeBinary>> {
    let source = match ReleaseSource::anthropic() {
        Ok(source) => source,
        Err(error) => return Task::ready(Err(error)),
    };
    let installer = Installer {
        // The shared client, not an IDE-lane one: this download is exactly what
        // the allowlist is there to gate.
        http: cx.http_client(),
        wsl: Arc::new(SystemWsl),
        executor: cx.background_executor().clone(),
        storage_dir: paths::data_dir().join("managed_agents").join("claude-code"),
        source: Arc::new(source),
        state_lock: cx.default_global::<InFlightInstalls>().state_lock.clone(),
    };
    window.spawn(cx, async move |cx| {
        let (progress_tx, mut progress_rx) = mpsc::unbounded();
        let _forward_progress = cx.spawn(async move |cx| {
            while let Some(update) = progress_rx.next().await {
                if cx
                    .update(|window, cx| on_progress(&update, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let progress: ProgressSink = Arc::new(move |update| {
            // Only fails once the forwarding task is gone, i.e. the window closed.
            progress_tx.unbounded_send(update).ok();
        });
        let mut ui = WindowUi { cx: cx.clone() };
        installer.run(distro, goal, &mut ui, progress).await
    })
}

type ProgressSink = Arc<dyn Fn(ManagedClaudeProgress) + Send + Sync>;

enum Goal {
    Ensure,
    Update(ClaudeReleaseChannel),
}

struct ReleaseSource {
    releases_url: String,
    signing_key: RsaPublicKey,
    signing_key_fingerprint: [u8; 20],
    pinned_version: Version,
}

impl ReleaseSource {
    fn anthropic() -> Result<Self> {
        let modulus = BASE64
            .decode(RELEASE_SIGNING_KEY_MODULUS)
            .context("decoding the embedded release signing key")?;
        let signing_key = RsaPublicKey::new(
            BigUint::from_bytes_be(&modulus),
            BigUint::from_bytes_be(&RELEASE_SIGNING_KEY_EXPONENT),
        )
        .context("loading the embedded release signing key")?;
        let mut signing_key_fingerprint = [0; 20];
        hex::decode_to_slice(
            RELEASE_SIGNING_KEY_FINGERPRINT,
            &mut signing_key_fingerprint,
        )
        .context("decoding the pinned release key fingerprint")?;
        Ok(Self {
            releases_url: RELEASES_URL.to_string(),
            signing_key,
            signing_key_fingerprint,
            pinned_version: PINNED_VERSION,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.releases_url)
    }

    fn required_hosts(&self) -> Vec<String> {
        hosts_required_to_download(&self.releases_url)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// Only the canonical `major.minor.patch` spelling: the text goes into URLs,
    /// and has to compare equal to the version the signed manifest names.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let number = |part: &str| -> Option<u32> {
            let canonical = !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && !(part.len() > 1 && part.starts_with('0'));
            if canonical { part.parse().ok() } else { None }
        };
        Some(Self {
            major: number(major)?,
            minor: number(minor)?,
            patch: number(patch)?,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxPlatform {
    X64,
    Arm64,
}

impl LinuxPlatform {
    fn from_machine(machine: &str) -> Result<Self> {
        match machine.trim() {
            "x86_64" => Ok(Self::X64),
            "aarch64" => Ok(Self::Arm64),
            other => bail!(
                "WSL のアーキテクチャ「{other}」向けの Linux 版 Claude Code はありません (対応しているのは x86_64 と aarch64 です)。"
            ),
        }
    }

    /// The glibc builds. The bucket's `-musl` ones are for musl distributions.
    fn key(self) -> &'static str {
        match self {
            Self::X64 => "linux-x64",
            Self::Arm64 => "linux-arm64",
        }
    }
}

/// Reads what `PROBE_SCRIPT` printed. The x64 build needs AVX2 (Bun's default
/// x64 target) and every build needs glibc; without them the CLI dies as it
/// starts, so such a distro is refused before anything is fetched.
fn platform_from_probe(output: &str) -> Result<LinuxPlatform> {
    let mut machine = None;
    let mut cpu_flags = "";
    let mut libc = None;
    for line in output.lines() {
        let Some(answer) = line.trim_end().strip_prefix(PROBE_MARKER) else {
            continue;
        };
        let (key, value) = answer.split_once(' ').unwrap_or((answer, ""));
        match key {
            "machine" => machine = Some(value.trim()),
            "cpu-flags" => cpu_flags = value,
            "libc" => libc = Some(value.trim()),
            _ => {}
        }
    }
    let platform = LinuxPlatform::from_machine(machine.context(PROBE_UNREADABLE)?)?;
    ensure!(libc != Some("musl"), NOT_GLIBC);
    if platform == LinuxPlatform::X64 {
        ensure!(
            cpu_flags.split_whitespace().any(|flag| flag == "avx2"),
            NO_AVX2
        );
    }
    Ok(platform)
}

#[derive(Deserialize)]
struct ReleaseManifest {
    version: String,
    platforms: BTreeMap<String, ManifestPlatform>,
}

#[derive(Deserialize)]
struct ManifestPlatform {
    binary: String,
    checksum: String,
    size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedFile {
    sha256: String,
    size: u64,
}

impl ExpectedFile {
    fn from_manifest(entry: &ManifestPlatform) -> Result<Self> {
        let sha256 = entry.checksum.to_ascii_lowercase();
        ensure!(
            is_sha256(&sha256),
            "checksum {:?} is not a SHA-256",
            entry.checksum
        );
        ensure!(
            (1..=MAX_BINARY_BYTES).contains(&entry.size),
            "size {} is out of range",
            entry.size
        );
        Ok(Self {
            sha256,
            size: entry.size,
        })
    }
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Debug)]
struct VerifiedRelease {
    binary: ExpectedFile,
    compressed: ExpectedFile,
}

/// A refusal whose Japanese reason is what the user sees, with the English
/// detail kept in the chain (and the log) for whoever investigates.
fn refuse(reason: &'static str, detail: String) -> anyhow::Error {
    log::warn!("claude-code: {detail}");
    anyhow!(detail).context(reason)
}

/// ASCII armor (RFC 4880 §6.2): exactly one `label` block and nothing around it.
/// RFC 9580 makes the CRC-24 line optional; when it is there it has to match.
fn dearmor(text: &str, label: &str) -> Result<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut lines = text.lines().map(str::trim_end);
    let first = lines
        .by_ref()
        .find(|line| !line.is_empty())
        .context("the armor is empty")?;
    ensure!(first == begin, "the armor does not start with {begin}");
    let mut encoded = String::new();
    let mut checksum = None;
    let mut in_headers = true;
    let mut ended = false;
    for line in lines.by_ref() {
        if in_headers {
            // Base64 has no colon, so a line with one is an armor header.
            if line.contains(':') {
                continue;
            }
            in_headers = false;
        }
        if line.is_empty() {
            continue;
        }
        if line == end {
            ended = true;
            break;
        }
        ensure!(checksum.is_none(), "the armor has data after its checksum");
        match line.strip_prefix('=') {
            Some(crc) => checksum = Some(crc),
            None => encoded.push_str(line.trim_start()),
        }
    }
    ensure!(ended, "the armor does not end with {end}");
    ensure!(
        lines.all(|line| line.trim().is_empty()),
        "the armor has data after {end}"
    );
    let data = BASE64
        .decode(&encoded)
        .context("the armor is not valid base64")?;
    if let Some(crc) = checksum {
        let expected = BASE64
            .decode(crc)
            .context("the armor checksum is not valid base64")?;
        let actual = crc24(&data).to_be_bytes();
        ensure!(
            actual.get(1..) == Some(expected.as_slice()),
            "the armor checksum does not match"
        );
    }
    Ok(data)
}

fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0x00B7_04CE;
    for byte in data {
        crc ^= u32::from(*byte) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4CFB;
            }
        }
    }
    crc & 0x00FF_FFFF
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .context("length overflow")?;
        let taken = self.bytes.get(self.position..end).with_context(|| {
            format!(
                "truncated: {count} more bytes expected at offset {}",
                self.position
            )
        })?;
        self.position = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8> {
        let [byte] = self.take(1)? else {
            bail!("truncated");
        };
        Ok(*byte)
    }

    fn u16(&mut self) -> Result<u16> {
        let [high, low] = self.take(2)? else {
            bail!("truncated");
        };
        Ok(u16::from_be_bytes([*high, *low]))
    }

    fn u32(&mut self) -> Result<u32> {
        let [first, second, third, fourth] = self.take(4)? else {
            bail!("truncated");
        };
        Ok(u32::from_be_bytes([*first, *second, *third, *fourth]))
    }

    fn consumed(&self) -> &'a [u8] {
        self.bytes.get(..self.position).unwrap_or_default()
    }

    fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.position..).unwrap_or_default()
    }

    fn is_empty(&self) -> bool {
        self.position >= self.bytes.len()
    }
}

/// One OpenPGP packet (RFC 4880 §4.2), in either header format: its tag, its
/// body, and whatever follows it. Partial and indeterminate lengths, which only
/// streamed data uses, are refused.
fn read_packet(bytes: &[u8]) -> Result<(u8, &[u8], &[u8])> {
    let mut reader = Reader::new(bytes);
    let header = reader.byte()?;
    ensure!(header & 0x80 != 0, "not an OpenPGP packet");
    let (tag, length) = if header & 0x40 != 0 {
        let first = usize::from(reader.byte()?);
        let length = match first {
            0..=191 => first,
            192..=223 => ((first - 192) << 8) + usize::from(reader.byte()?) + 192,
            255 => usize::try_from(reader.u32()?)?,
            _ => bail!("partial body lengths are not accepted"),
        };
        (header & 0x3f, length)
    } else {
        let length = match header & 0x03 {
            0 => usize::from(reader.byte()?),
            1 => usize::from(reader.u16()?),
            2 => usize::try_from(reader.u32()?)?,
            _ => bail!("indeterminate packet lengths are not accepted"),
        };
        ((header >> 2) & 0x0f, length)
    };
    ensure!(
        length <= MAX_PACKET_BYTES,
        "packet length {length} is out of range"
    );
    let body = reader.take(length)?;
    Ok((tag, body, reader.rest()))
}

/// A multiprecision integer (RFC 4880 §3.2), accepted only in its one canonical
/// encoding.
fn read_mpi<'a>(reader: &mut Reader<'a>) -> Result<&'a [u8]> {
    let bits = usize::from(reader.u16()?);
    let value = reader.take(bits.div_ceil(8))?;
    let Some(first) = value.first() else {
        bail!("empty integer");
    };
    let actual_bits = 8 * (value.len() - 1) + (8 - first.leading_zeros() as usize);
    ensure!(
        actual_bits == bits,
        "integer declares {bits} bits but has {actual_bits}"
    );
    Ok(value)
}

struct Subpacket<'a> {
    kind: u8,
    critical: bool,
    data: &'a [u8],
}

/// A signature subpacket area (RFC 4880 §5.2.3.1).
fn read_subpackets(area: &[u8]) -> Result<Vec<Subpacket<'_>>> {
    let mut reader = Reader::new(area);
    let mut subpackets = Vec::new();
    while !reader.is_empty() {
        let first = usize::from(reader.byte()?);
        let length = match first {
            0..=191 => first,
            192..=254 => ((first - 192) << 8) + usize::from(reader.byte()?) + 192,
            _ => usize::try_from(reader.u32()?)?,
        };
        let content = reader.take(length)?;
        let Some((&kind, data)) = content.split_first() else {
            bail!("empty signature subpacket");
        };
        subpackets.push(Subpacket {
            kind: kind & 0x7f,
            critical: kind & 0x80 != 0,
            data,
        });
    }
    Ok(subpackets)
}

/// A critical subpacket that is not understood makes the signature invalid
/// (RFC 4880 §5.2.3.1); the issuer, when named, has to be the pinned key; and a
/// signature past its expiration time, counted from its creation, is refused.
fn check_hashed_subpackets(area: &[u8], pinned_fingerprint: &[u8; 20], now: u64) -> Result<()> {
    let mut created = None;
    let mut expires_after = None;
    for subpacket in read_subpackets(area)? {
        match subpacket.kind {
            SUBPACKET_CREATION_TIME => created = Some(subpacket_time(subpacket.data)?),
            SUBPACKET_EXPIRATION_TIME => expires_after = Some(subpacket_time(subpacket.data)?),
            SUBPACKET_ISSUER_FINGERPRINT => {
                let [4, fingerprint @ ..] = subpacket.data else {
                    bail!("the issuer fingerprint is not a version 4 fingerprint");
                };
                ensure!(
                    fingerprint == pinned_fingerprint,
                    "signed by {}, not by the pinned key {}",
                    hex::encode_upper(fingerprint),
                    hex::encode_upper(pinned_fingerprint)
                );
            }
            SUBPACKET_ISSUER_KEY_ID => ensure!(
                pinned_fingerprint.get(12..) == Some(subpacket.data),
                "signed by key ID {}, not by the pinned key",
                hex::encode_upper(subpacket.data)
            ),
            kind if subpacket.critical => {
                bail!("critical signature subpacket {kind} is not understood")
            }
            _ => {}
        }
    }
    // Zero means the signature does not expire.
    if let Some(expires_after) = expires_after.filter(|seconds| *seconds != 0) {
        let created = created.context("the signature expires but has no creation time")?;
        let expires_at = u64::from(created) + u64::from(expires_after);
        ensure!(
            now < expires_at,
            "the signature expired at {expires_at} (Unix time)"
        );
    }
    Ok(())
}

fn subpacket_time(data: &[u8]) -> Result<u32> {
    let [first, second, third, fourth] = data else {
        bail!("malformed signature time");
    };
    Ok(u32::from_be_bytes([*first, *second, *third, *fourth]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignatureHash {
    Sha256,
    Sha384,
    Sha512,
}

impl SignatureHash {
    fn from_id(id: u8) -> Option<Self> {
        match id {
            8 => Some(Self::Sha256),
            9 => Some(Self::Sha384),
            10 => Some(Self::Sha512),
            _ => None,
        }
    }

    fn digest(self, parts: &[&[u8]]) -> Vec<u8> {
        fn digest_with<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
            let mut hasher = D::new();
            for part in parts {
                hasher.update(part);
            }
            hasher.finalize().to_vec()
        }
        match self {
            Self::Sha256 => digest_with::<Sha256>(parts),
            Self::Sha384 => digest_with::<Sha384>(parts),
            Self::Sha512 => digest_with::<Sha512>(parts),
        }
    }

    fn padding(self) -> Pkcs1v15Sign {
        match self {
            Self::Sha256 => Pkcs1v15Sign::new::<Sha256>(),
            Self::Sha384 => Pkcs1v15Sign::new::<Sha384>(),
            Self::Sha512 => Pkcs1v15Sign::new::<Sha512>(),
        }
    }
}

struct SignaturePacket<'a> {
    hash: SignatureHash,
    /// Version through the end of the hashed subpackets: what the signature
    /// covers besides the document itself.
    hashed_part: &'a [u8],
    quick_check: &'a [u8],
    value: &'a [u8],
}

/// Exactly one version 4 signature packet (RFC 4880 §5.2.3), of the only kind
/// accepted: RSA over a binary document with SHA-256, SHA-384 or SHA-512.
fn parse_signature<'a>(
    packet: &'a [u8],
    pinned_fingerprint: &[u8; 20],
    now: u64,
) -> Result<SignaturePacket<'a>> {
    let (tag, body, rest) = read_packet(packet)?;
    ensure!(
        tag == SIGNATURE_PACKET_TAG,
        "expected a signature packet, found packet tag {tag}"
    );
    ensure!(
        rest.is_empty(),
        "{} bytes follow the signature packet",
        rest.len()
    );
    let mut reader = Reader::new(body);
    let version = reader.byte()?;
    ensure!(
        version == SIGNATURE_VERSION,
        "signature version {version} is not accepted"
    );
    let signature_type = reader.byte()?;
    ensure!(
        signature_type == BINARY_DOCUMENT_SIGNATURE,
        "signature type {signature_type:#04x} is not a binary document signature"
    );
    let algorithm = reader.byte()?;
    ensure!(
        algorithm == RSA_ALGORITHM,
        "public-key algorithm {algorithm} is not RSA"
    );
    let hash_id = reader.byte()?;
    let hash = SignatureHash::from_id(hash_id)
        .with_context(|| format!("hash algorithm {hash_id} is not SHA-256, SHA-384 or SHA-512"))?;
    let hashed_length = usize::from(reader.u16()?);
    check_hashed_subpackets(reader.take(hashed_length)?, pinned_fingerprint, now)?;
    let hashed_part = reader.consumed();
    let unhashed_length = usize::from(reader.u16()?);
    // Nothing in the unhashed area is signed (it usually repeats the issuer),
    // so it only has to be well formed.
    read_subpackets(reader.take(unhashed_length)?)?;
    let quick_check = reader.take(2)?;
    let value = read_mpi(&mut reader)?;
    ensure!(
        reader.is_empty(),
        "{} bytes follow the signature value",
        reader.rest().len()
    );
    Ok(SignaturePacket {
        hash,
        hashed_part,
        quick_check,
        value,
    })
}

/// RFC 4880 §5.2.4: the digest runs over the document, the hashed part of the
/// signature and a trailer with that part's length; the RSA signature is
/// EMSA-PKCS1-v1_5 over it.
fn verify_packet(
    source: &ReleaseSource,
    content: &[u8],
    packet: &[u8],
    now: SystemTime,
) -> Result<()> {
    let now = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let signature = parse_signature(packet, &source.signing_key_fingerprint, now)?;
    let hashed_length = u32::try_from(signature.hashed_part.len())?;
    let digest = signature.hash.digest(&[
        content,
        signature.hashed_part,
        &SIGNATURE_TRAILER_PREFIX,
        &hashed_length.to_be_bytes(),
    ]);
    ensure!(
        digest.get(..2) == Some(signature.quick_check),
        "the signature's quick check does not match the content"
    );
    // The integer drops leading zero bytes; the RSA check wants the full width.
    let modulus_length = source.signing_key.size();
    ensure!(
        signature.value.len() <= modulus_length,
        "the signature is longer than the key"
    );
    let mut padded = vec![0; modulus_length - signature.value.len()];
    padded.extend_from_slice(signature.value);
    source
        .signing_key
        .verify(signature.hash.padding(), &digest, &padded)
        .context("the RSA signature does not verify")?;
    Ok(())
}

fn verify_signature(
    source: &ReleaseSource,
    content: &[u8],
    armored_signature: &[u8],
    now: SystemTime,
) -> Result<()> {
    let armored_signature =
        std::str::from_utf8(armored_signature).context("the signature is not ASCII armor")?;
    let packet = dearmor(armored_signature, "PGP SIGNATURE")?;
    verify_packet(source, content, &packet, now)
}

/// What `platform`'s file in the manifest must hash to, once the manifest has
/// passed its signature check and proved to be for `version`.
fn verified_manifest_entry(
    source: &ReleaseSource,
    version: Version,
    platform: LinuxPlatform,
    manifest: &[u8],
    signature: &[u8],
    file_name: &str,
    now: SystemTime,
) -> Result<ExpectedFile> {
    verify_signature(source, manifest, signature, now)
        .map_err(|error| refuse(SIGNATURE_REFUSED, format!("{error:#}")))?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(manifest).context("リリース情報を読み取れませんでした。")?;
    if manifest.version != version.to_string() {
        bail!(
            "リリース情報の版 ({}) が要求した版 ({version}) と一致しません。中止しました。",
            manifest.version
        );
    }
    let entry = manifest.platforms.get(platform.key()).with_context(|| {
        format!(
            "リリース情報に {} 向けのファイルがありません。",
            platform.key()
        )
    })?;
    ensure!(
        entry.binary == file_name,
        "リリース情報の {} のファイル名 ({}) が想定と違います。",
        platform.key(),
        entry.binary
    );
    ExpectedFile::from_manifest(entry).context("リリース情報を読み取れませんでした。")
}

/// `future`, given up once `IDLE_TIMEOUT` passes without it finishing. Every
/// wait on the network goes through this, so that one stuck connection cannot
/// hold an install (and everyone waiting on it) forever.
async fn within_idle_timeout<T>(
    executor: &BackgroundExecutor,
    url: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut timeout = executor.timer(IDLE_TIMEOUT).fuse();
    let mut future = std::pin::pin!(future.fuse());
    futures::select_biased! {
        result = future => result,
        _ = timeout => Err(refuse(
            STALLED,
            format!("{url} sent nothing for {} seconds", IDLE_TIMEOUT.as_secs()),
        )),
    }
}

/// A GET on the release bucket. Redirects are refused rather than followed:
/// the bucket serves every file itself, and following one would reach a host
/// the user was never asked about.
async fn get_release_file(
    http: &dyn HttpClient,
    executor: &BackgroundExecutor,
    url: &str,
) -> Result<Response<AsyncBody>> {
    let response = within_idle_timeout(executor, url, async {
        http.get(url, AsyncBody::default(), false)
            .await
            .with_context(|| format!("{url} を取得できませんでした。"))
    })
    .await?;
    let status = response.status();
    if status.is_redirection() {
        let target = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        return Err(refuse(
            REDIRECT_REFUSED,
            format!("{url} redirected to {target:?}"),
        ));
    }
    ensure!(
        status.is_success(),
        "{url} を取得できませんでした (HTTP {status})。"
    );
    Ok(response)
}

async fn fetch_small(
    http: &dyn HttpClient,
    executor: &BackgroundExecutor,
    url: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    log::info!("claude-code: fetching {url}");
    let mut response = get_release_file(http, executor, url).await?;
    let mut body = Vec::new();
    within_idle_timeout(executor, url, async {
        response
            .body_mut()
            .take(limit + 1)
            .read_to_end(&mut body)
            .await
            .with_context(|| format!("{url} を取得できませんでした。"))
    })
    .await?;
    ensure!(
        body.len() as u64 <= limit,
        "{url} が想定より大きいため中止しました。"
    );
    Ok(body)
}

async fn fetch_manifest_entry(
    http: &dyn HttpClient,
    executor: &BackgroundExecutor,
    source: &ReleaseSource,
    version: Version,
    platform: LinuxPlatform,
    manifest_name: &str,
    file_name: &str,
) -> Result<ExpectedFile> {
    let manifest_url = source.url(&format!("{version}/{manifest_name}"));
    let manifest = fetch_small(http, executor, &manifest_url, MAX_MANIFEST_BYTES).await?;
    let signature_url = format!("{manifest_url}.sig");
    let signature = fetch_small(http, executor, &signature_url, MAX_SIGNATURE_BYTES).await?;
    verified_manifest_entry(
        source,
        version,
        platform,
        &manifest,
        &signature,
        file_name,
        SystemTime::now(),
    )
}

/// Both manifests: the plain one for the binary that gets staged, the `.zst`
/// one for the download, so that neither file is ever trusted unchecked.
async fn fetch_release(
    http: Arc<dyn HttpClient>,
    source: Arc<ReleaseSource>,
    executor: BackgroundExecutor,
    version: Version,
    platform: LinuxPlatform,
) -> Result<VerifiedRelease> {
    let binary = fetch_manifest_entry(
        &*http,
        &executor,
        &source,
        version,
        platform,
        "manifest.json",
        BINARY_NAME,
    )
    .await?;
    let compressed = fetch_manifest_entry(
        &*http,
        &executor,
        &source,
        version,
        platform,
        "manifest.zst.json",
        COMPRESSED_BINARY_NAME,
    )
    .await?;
    Ok(VerifiedRelease { binary, compressed })
}

fn check_file(description: &str, size: u64, hasher: Sha256, expected: &ExpectedFile) -> Result<()> {
    if size != expected.size {
        return Err(refuse(
            SIZE_REFUSED,
            format!(
                "{description} is {size} bytes, its manifest lists {}",
                expected.size
            ),
        ));
    }
    let sha256 = format!("{:x}", hasher.finalize());
    if sha256 != expected.sha256 {
        return Err(refuse(
            CHECKSUM_REFUSED,
            format!(
                "{description} has SHA-256 {sha256}, its manifest lists {}",
                expected.sha256
            ),
        ));
    }
    Ok(())
}

/// A local file operation that failed, reported in Japanese with the English
/// detail kept for the log.
fn io_failure(
    reason: &'static str,
    action: &str,
    path: &Path,
    error: impl fmt::Display,
) -> anyhow::Error {
    refuse(reason, format!("{action} {}: {error}", path.display()))
}

async fn download_verified(
    http: &dyn HttpClient,
    executor: &BackgroundExecutor,
    url: &str,
    destination: &Path,
    expected: &ExpectedFile,
    progress: &ProgressSink,
) -> Result<()> {
    log::info!("claude-code: downloading {url}");
    let mut response = get_release_file(http, executor, url).await?;
    let mut file = std::fs::File::create(destination)
        .map_err(|error| io_failure(SAVE_FAILED, "creating", destination, error))?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut reported_percent = None;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = within_idle_timeout(executor, url, async {
            response
                .body_mut()
                .read(&mut buffer)
                .await
                .with_context(|| format!("{url} を取得できませんでした。"))
        })
        .await?;
        if read == 0 {
            break;
        }
        let Some(chunk) = buffer.get(..read) else {
            return Err(refuse(
                SAVE_FAILED,
                format!("{url}: the response body overran its buffer"),
            ));
        };
        received += read as u64;
        if received > expected.size {
            return Err(refuse(
                SIZE_REFUSED,
                format!(
                    "{url} is longer than the {} bytes its manifest lists",
                    expected.size
                ),
            ));
        }
        hasher.update(chunk);
        file.write_all(chunk)
            .map_err(|error| io_failure(SAVE_FAILED, "writing", destination, error))?;
        let percent = received * 100 / expected.size;
        if reported_percent != Some(percent) {
            reported_percent = Some(percent);
            progress(ManagedClaudeProgress::Downloading {
                received,
                total: expected.size,
            });
        }
    }
    file.sync_all()
        .map_err(|error| io_failure(SAVE_FAILED, "writing", destination, error))?;
    check_file(url, received, hasher, expected)
}

/// Streams the download through zstd to disk, stopping as soon as the output
/// outgrows what the manifest lists.
fn decompress_verified(
    compressed: &Path,
    destination: &Path,
    expected: &ExpectedFile,
) -> Result<()> {
    let input = std::fs::File::open(compressed)
        .map_err(|error| io_failure(EXTRACT_FAILED, "opening", compressed, error))?;
    let mut decoder = zstd::stream::read::Decoder::new(input)
        .map_err(|error| io_failure(EXTRACT_FAILED, "starting zstd for", compressed, error))?;
    let mut output = std::fs::File::create(destination)
        .map_err(|error| io_failure(SAVE_FAILED, "creating", destination, error))?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|error| io_failure(EXTRACT_FAILED, "decompressing", compressed, error))?;
        if read == 0 {
            break;
        }
        let Some(chunk) = buffer.get(..read) else {
            return Err(io_failure(
                EXTRACT_FAILED,
                "decompressing",
                compressed,
                "zstd overran its buffer",
            ));
        };
        written += read as u64;
        if written > expected.size {
            return Err(refuse(
                SIZE_REFUSED,
                format!(
                    "{} decompresses to more than the {} bytes its manifest lists",
                    compressed.display(),
                    expected.size
                ),
            ));
        }
        hasher.update(chunk);
        output
            .write_all(chunk)
            .map_err(|error| io_failure(SAVE_FAILED, "writing", destination, error))?;
    }
    output
        .sync_all()
        .map_err(|error| io_failure(SAVE_FAILED, "writing", destination, error))?;
    check_file(
        &destination.display().to_string(),
        written,
        hasher,
        expected,
    )
}

/// The download and both checks, in a fresh directory that the caller removes
/// whatever happens: only a file that passed both checks is named `claude`.
async fn download_binary(
    http: Arc<dyn HttpClient>,
    executor: BackgroundExecutor,
    url: String,
    release: VerifiedRelease,
    work_dir: PathBuf,
    progress: ProgressSink,
) -> Result<PathBuf> {
    let compressed_path = work_dir.join(format!("{COMPRESSED_BINARY_NAME}.partial"));
    download_verified(
        &*http,
        &executor,
        &url,
        &compressed_path,
        &release.compressed,
        &progress,
    )
    .await?;
    progress(ManagedClaudeProgress::Extracting);
    let partial_path = work_dir.join(format!("{BINARY_NAME}.partial"));
    decompress_verified(&compressed_path, &partial_path, &release.binary)?;
    std::fs::remove_file(&compressed_path)
        .map_err(|error| io_failure(SAVE_FAILED, "removing", &compressed_path, error))?;
    let binary_path = work_dir.join(BINARY_NAME);
    std::fs::rename(&partial_path, &binary_path)
        .map_err(|error| io_failure(SAVE_FAILED, "renaming", &partial_path, error))?;
    Ok(binary_path)
}

/// Removes what interrupted downloads left behind. A download that is still
/// running keeps writing its files, so an entry is judged by the newest time in
/// it rather than by the folder's own.
fn sweep_stale_downloads(downloads_dir: &Path, now: SystemTime) {
    let entries = match std::fs::read_dir(downloads_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            log::warn!(
                "claude-code: cannot list {}: {error}",
                downloads_dir.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(newest) = newest_modification(&path) else {
            continue;
        };
        if now
            .duration_since(newest)
            .is_ok_and(|age| age > STALE_DOWNLOAD_AGE)
        {
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            removed
                .with_context(|| format!("removing the stale download {}", path.display()))
                .log_err();
        }
    }
}

fn newest_modification(path: &Path) -> Option<SystemTime> {
    let own = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()?;
    let children = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        });
    Some(children.fold(own, SystemTime::max))
}

/// Kept in Zed's data directory, which the sandbox can neither read nor write
/// (ADR-F49 decision 11).
#[derive(Debug, Default, Serialize, Deserialize)]
struct InstallerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    installed_version: Option<String>,
    #[serde(default)]
    consented_versions: BTreeSet<String>,
    /// `<version>/<platform>` → a binary that matched its signed manifest, so
    /// an intact staged copy can be found again without asking the network.
    #[serde(default)]
    verified_binaries: BTreeMap<String, VerifiedBinary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct VerifiedBinary {
    sha256: String,
    /// Fingerprint of the release key the manifest was checked against. A
    /// record made under any other key is not reused.
    signing_key: String,
}

impl InstallerState {
    fn installed_version(&self) -> Option<Version> {
        self.installed_version.as_deref().and_then(Version::parse)
    }

    fn verified_sha256(
        &self,
        version: Version,
        platform: LinuxPlatform,
        signing_key: &str,
    ) -> Option<String> {
        self.verified_binaries
            .get(&format!("{version}/{}", platform.key()))
            .filter(|verified| verified.signing_key == signing_key && is_sha256(&verified.sha256))
            .map(|verified| verified.sha256.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConsentRequest {
    Install {
        version: Version,
    },
    Update {
        from: Version,
        to: Version,
    },
    CheckForUpdate {
        channel: ClaudeReleaseChannel,
    },
    /// The release host was refused mid-way: it left the allowlist after the
    /// user agreed to the download.
    AllowHosts,
}

struct ConsentPrompt {
    message: String,
    detail: String,
    confirm: &'static str,
}

/// Worded like the ADR-F12 install consent banner (GLOSSARY「エージェント CLI
/// のインストール同意」), so fetching this CLI reads as the same decision.
fn consent_prompt(request: &ConsentRequest, hosts: &[String], missing: &[String]) -> ConsentPrompt {
    let hosts_line = format!(
        "許可が必要なホスト: {} ({})",
        hosts.join("、"),
        if missing.is_empty() {
            "登録済み"
        } else {
            "未登録"
        }
    );
    let adds_hosts = !missing.is_empty();
    match request {
        ConsentRequest::Install { version } => ConsentPrompt {
            message: format!("Linux 版 Claude Code v{version} を取得します"),
            detail: format!(
                "サンドボックス (WSL) で実行する Claude Code を Anthropic の配布元から取得し、署名とハッシュを確認してから WSL に配置します。\n{hosts_line}"
            ),
            confirm: if adds_hosts {
                "許可リストに追加してインストール"
            } else {
                "了承してインストール"
            },
        },
        ConsentRequest::Update { from, to } => ConsentPrompt {
            message: format!("Claude Code を v{to} に更新します"),
            detail: format!(
                "現在の版: v{from}\n署名とハッシュを確認してから WSL に配置します。\n{hosts_line}"
            ),
            confirm: if adds_hosts {
                "許可リストに追加して更新"
            } else {
                "了承して更新"
            },
        },
        ConsentRequest::CheckForUpdate { channel } => ConsentPrompt {
            message: "Claude Code の更新を確認します".to_string(),
            detail: format!(
                "{}の版番号を Anthropic の配布元から読み取ります。\n{hosts_line}",
                channel.label()
            ),
            confirm: "許可リストに追加して確認",
        },
        ConsentRequest::AllowHosts => ConsentPrompt {
            message: "Claude Code の取得に許可リストへの追加が必要です".to_string(),
            detail: hosts_line,
            confirm: "許可リストに追加して続行",
        },
    }
}

trait InstallerUi {
    /// The subset of `hosts` the running egress policy does not allow yet.
    fn missing_hosts(&mut self, hosts: &[String]) -> Result<Vec<String>>;
    fn confirm(
        &mut self,
        request: &ConsentRequest,
        hosts: &[String],
        missing: &[String],
    ) -> LocalBoxFuture<'static, Result<bool>>;
    /// Adds `hosts` to the allowlist and returns once requests to them pass.
    fn add_allowed_hosts(&mut self, hosts: Vec<String>) -> LocalBoxFuture<'static, Result<()>>;
}

trait WslAgentHost {
    /// What `PROBE_SCRIPT` prints inside `distro`.
    fn probe(&self, distro: Option<String>) -> LocalBoxFuture<'static, Result<String>>;
    fn staged(
        &self,
        distro: Option<String>,
        sha256: String,
    ) -> LocalBoxFuture<'static, Result<Option<String>>>;
    fn stage(
        &self,
        distro: Option<String>,
        source: PathBuf,
        sha256: String,
    ) -> LocalBoxFuture<'static, Result<String>>;
}

struct Installer {
    http: Arc<dyn HttpClient>,
    wsl: Arc<dyn WslAgentHost>,
    executor: BackgroundExecutor,
    storage_dir: PathBuf,
    source: Arc<ReleaseSource>,
    state_lock: Arc<AsyncMutex<()>>,
}

impl Installer {
    async fn run(
        &self,
        distro: Option<String>,
        goal: Goal,
        ui: &mut dyn InstallerUi,
        progress: ProgressSink,
    ) -> Result<ManagedClaudeBinary> {
        let probe = self.wsl.probe(distro.clone()).await?;
        let platform = platform_from_probe(&probe)?;
        let downloads_dir = self.storage_dir.join("downloads");
        self.executor
            .spawn({
                let downloads_dir = downloads_dir.clone();
                async move { sweep_stale_downloads(&downloads_dir, SystemTime::now()) }
            })
            .await;
        let state = self.load_state().await;
        let recorded = state.installed_version();
        // The pinned version is a floor: a fork release that raises it reaches
        // existing installs, and a rolled-back record cannot bring back an
        // older release.
        let installed = recorded.map_or(self.source.pinned_version, |recorded| {
            recorded.max(self.source.pinned_version)
        });
        let version = match goal {
            Goal::Ensure => installed,
            Goal::Update(channel) => {
                let offered = self.read_release_pointer(channel, ui).await?;
                if offered < installed {
                    bail!(
                        "{}の v{offered} はインストール済みの v{installed} より古いため、更新しません。",
                        channel.label()
                    );
                }
                offered
            }
        };
        let consent = match recorded {
            Some(recorded) if recorded < version => ConsentRequest::Update {
                from: recorded,
                to: version,
            },
            _ => ConsentRequest::Install { version },
        };
        let signing_key = hex::encode_upper(self.source.signing_key_fingerprint);

        progress(ManagedClaudeProgress::CheckingStaged);
        if let Some(sha256) = state.verified_sha256(version, platform, &signing_key)
            && let Some(path_in_wsl) = self.staged(distro.clone(), sha256.clone()).await?
        {
            return Ok(self.finish(version, platform, sha256, path_in_wsl).await);
        }

        self.obtain_consent(&state, version, &consent, ui).await?;

        progress(ManagedClaudeProgress::Verifying);
        let release = self
            .with_egress_consent(ui, || {
                self.executor.spawn(fetch_release(
                    self.http.clone(),
                    self.source.clone(),
                    self.executor.clone(),
                    version,
                    platform,
                ))
            })
            .await?;
        let sha256 = release.binary.sha256.clone();
        // The record of it may be gone while the binary itself stayed in WSL.
        if let Some(path_in_wsl) = self.staged(distro.clone(), sha256.clone()).await? {
            return Ok(self.finish(version, platform, sha256, path_in_wsl).await);
        }

        let work_dir = self
            .executor
            .spawn(async move {
                std::fs::create_dir_all(&downloads_dir)?;
                tempfile::Builder::new()
                    .prefix("claude-")
                    .tempdir_in(&downloads_dir)
            })
            .await
            .context("ダウンロード先のフォルダーを作成できませんでした。")?;
        let download_url = self.source.url(&format!(
            "{version}/{}/{COMPRESSED_BINARY_NAME}",
            platform.key()
        ));
        let binary_path = self
            .with_egress_consent(ui, || {
                self.executor.spawn(download_binary(
                    self.http.clone(),
                    self.executor.clone(),
                    download_url.clone(),
                    release.clone(),
                    work_dir.path().to_path_buf(),
                    progress.clone(),
                ))
            })
            .await?;

        progress(ManagedClaudeProgress::Staging);
        let path_in_wsl = self
            .wsl
            .stage(distro, binary_path, sha256.clone())
            .await
            .map_err(|error| refuse(STAGING_FAILED, format!("{error:#}")))?;
        self.executor
            .spawn(async move { work_dir.close() })
            .await
            .context("removing the downloaded Claude Code")
            .log_err();
        Ok(self.finish(version, platform, sha256, path_in_wsl).await)
    }

    async fn finish(
        &self,
        version: Version,
        platform: LinuxPlatform,
        sha256: String,
        path_in_wsl: String,
    ) -> ManagedClaudeBinary {
        let signing_key = hex::encode_upper(self.source.signing_key_fingerprint);
        self.update_state(|state| {
            // Only ever forward: an ensure that began before an update finished
            // must not put the older version back.
            if state
                .installed_version()
                .is_none_or(|installed| installed < version)
            {
                state.installed_version = Some(version.to_string());
            }
            state.verified_binaries.insert(
                format!("{version}/{}", platform.key()),
                VerifiedBinary {
                    sha256: sha256.clone(),
                    signing_key,
                },
            );
        })
        .await;
        ManagedClaudeBinary {
            version: version.to_string(),
            sha256,
            path_in_wsl,
        }
    }

    async fn obtain_consent(
        &self,
        state: &InstallerState,
        version: Version,
        request: &ConsentRequest,
        ui: &mut dyn InstallerUi,
    ) -> Result<()> {
        let hosts = self.source.required_hosts();
        let missing = ui.missing_hosts(&hosts)?;
        // The allowlist is re-checked every time rather than trusted from the
        // earlier consent: the user may have removed the host since (ADR-F12).
        if state.consented_versions.contains(&version.to_string()) && missing.is_empty() {
            return Ok(());
        }
        if !ui.confirm(request, &hosts, &missing).await? {
            bail!(DOWNLOAD_DECLINED);
        }
        if !missing.is_empty() {
            ui.add_allowed_hosts(missing).await?;
        }
        self.update_state(|state| {
            state.consented_versions.insert(version.to_string());
        })
        .await;
        Ok(())
    }

    async fn read_release_pointer(
        &self,
        channel: ClaudeReleaseChannel,
        ui: &mut dyn InstallerUi,
    ) -> Result<Version> {
        let hosts = self.source.required_hosts();
        let missing = ui.missing_hosts(&hosts)?;
        if !missing.is_empty() {
            let request = ConsentRequest::CheckForUpdate { channel };
            if !ui.confirm(&request, &hosts, &missing).await? {
                bail!(UPDATE_CHECK_DECLINED);
            }
            ui.add_allowed_hosts(missing).await?;
        }
        let url = self.source.url(channel.pointer_name());
        let body = self
            .with_egress_consent(ui, || {
                let http = self.http.clone();
                let executor = self.executor.clone();
                let url = url.clone();
                self.executor.spawn(async move {
                    fetch_small(&*http, &executor, &url, MAX_POINTER_BYTES).await
                })
            })
            .await?;
        let text = std::str::from_utf8(&body).unwrap_or_default().trim();
        Version::parse(text).with_context(|| {
            format!(
                "{}の版番号 ({text:?}) を読み取れませんでした。",
                channel.label()
            )
        })
    }

    /// Runs `attempt`, and when the allowlist refused the release host (it can
    /// have been removed after the user agreed), asks to add it back and runs
    /// it once more.
    async fn with_egress_consent<T: 'static>(
        &self,
        ui: &mut dyn InstallerUi,
        mut attempt: impl FnMut() -> Task<Result<T>>,
    ) -> Result<T> {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(hosts) = egress_denied_hosts(&error) else {
            return Err(error);
        };
        // Nothing but the release host is ever requested (redirects are
        // refused), and this allowlist is also the sandbox's network ceiling,
        // so no other host is offered.
        let release_hosts = self.source.required_hosts();
        if !hosts.iter().all(|host| release_hosts.contains(host)) {
            return Err(error);
        }
        let missing = ui.missing_hosts(&hosts)?;
        if !missing.is_empty() {
            if !ui
                .confirm(&ConsentRequest::AllowHosts, &hosts, &missing)
                .await?
            {
                return Err(error.context(DOWNLOAD_DECLINED));
            }
            ui.add_allowed_hosts(missing).await?;
        }
        attempt().await
    }

    fn state_path(&self) -> PathBuf {
        self.storage_dir.join("state.json")
    }

    /// An unreadable record is treated as none: the worst that costs is a
    /// repeated question and a download that is verified all over again.
    async fn load_state(&self) -> InstallerState {
        let path = self.state_path();
        self.executor
            .spawn(async move {
                match std::fs::read(&path) {
                    Ok(contents) => serde_json::from_slice(&contents).unwrap_or_else(|error| {
                        log::warn!(
                            "claude-code: ignoring unreadable {}: {error}",
                            path.display()
                        );
                        InstallerState::default()
                    }),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        InstallerState::default()
                    }
                    Err(error) => {
                        log::warn!(
                            "claude-code: ignoring unreadable {}: {error}",
                            path.display()
                        );
                        InstallerState::default()
                    }
                }
            })
            .await
    }

    async fn staged(&self, distro: Option<String>, sha256: String) -> Result<Option<String>> {
        self.wsl
            .staged(distro, sha256)
            .await
            .map_err(|error| refuse(STAGED_CHECK_FAILED, format!("{error:#}")))
    }

    /// Read-modify-write under the lock every installer shares, so that an
    /// update and an ensure finishing side by side cannot undo each other.
    async fn update_state(&self, update: impl FnOnce(&mut InstallerState)) {
        let _guard = self.state_lock.lock().await;
        let mut state = self.load_state().await;
        update(&mut state);
        // The binary is in place either way; a lost record only costs a
        // signature check (and possibly a question) next time.
        self.save_state(&state).await.log_err();
    }

    async fn save_state(&self, state: &InstallerState) -> Result<()> {
        let path = self.state_path();
        let contents = serde_json::to_vec_pretty(state)?;
        self.executor
            .spawn(async move {
                let directory = path.parent().context("the state file has no directory")?;
                std::fs::create_dir_all(directory)?;
                // A uniquely named file renamed over the old one, so a reader
                // sees the previous record or the new one, never half of one.
                let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
                temporary.write_all(&contents)?;
                temporary.as_file().sync_all()?;
                temporary.persist(&path)?;
                anyhow::Ok(())
            })
            .await
            .with_context(|| format!("saving {}", self.state_path().display()))
    }
}

struct WindowUi {
    cx: AsyncWindowContext,
}

impl InstallerUi for WindowUi {
    fn missing_hosts(&mut self, hosts: &[String]) -> Result<Vec<String>> {
        let hosts = hosts
            .iter()
            .cloned()
            .map(SharedString::from)
            .collect::<Vec<_>>();
        self.cx.update(|_, cx| {
            missing_hosts(&hosts, cx)
                .into_iter()
                .map(|host| host.to_string())
                .collect()
        })
    }

    fn confirm(
        &mut self,
        request: &ConsentRequest,
        hosts: &[String],
        missing: &[String],
    ) -> LocalBoxFuture<'static, Result<bool>> {
        let prompt = consent_prompt(request, hosts, missing);
        let answer = self.cx.prompt(
            PromptLevel::Warning,
            &prompt.message,
            Some(&prompt.detail),
            &[
                PromptButton::ok(prompt.confirm),
                PromptButton::cancel(CANCEL_LABEL),
            ],
        );
        // A prompt that goes away with its window counts as declined.
        async move { Ok(answer.await.is_ok_and(|index| index == 0)) }.boxed_local()
    }

    /// The write and the wait are kept apart here, rather than taken from
    /// [`add_consented_hosts_and_await_pickup`] whole, so that a write that
    /// fails is still reported in the words the consent uses for it.
    fn add_allowed_hosts(&mut self, hosts: Vec<String>) -> LocalBoxFuture<'static, Result<()>> {
        let executor = self.cx.background_executor().clone();
        let prepared = self.cx.update(|_, cx| {
            let policy = cx
                .try_global::<egress_policy::EgressAllowlistGlobal>()
                .map(|global| global.0.clone());
            // Subscribed before the write so that the watcher's update cannot
            // slip in between.
            let changes = policy.as_ref().map(EgressPolicyHandle::subscribe);
            (add_consented_hosts(hosts.clone(), cx), policy, changes)
        });
        async move {
            let (write, policy, changes) = prepared?;
            write
                .await
                .map_err(|error| anyhow!(allowlist_write_error(&error)))?;
            let (Some(policy), Some(mut changes)) = (policy, changes) else {
                return Ok(());
            };
            await_allowlist_pickup(&hosts, &policy, &mut changes, &executor).await
        }
        .boxed_local()
    }
}

struct SystemWsl;

impl WslAgentHost for SystemWsl {
    fn probe(&self, distro: Option<String>) -> LocalBoxFuture<'static, Result<String>> {
        async move { wsl_probe(distro.as_deref()).await }.boxed_local()
    }

    fn staged(
        &self,
        distro: Option<String>,
        sha256: String,
    ) -> LocalBoxFuture<'static, Result<Option<String>>> {
        async move { staged_agent_binary(distro.as_deref(), BINARY_NAME, &sha256).await }
            .boxed_local()
    }

    fn stage(
        &self,
        distro: Option<String>,
        source: PathBuf,
        sha256: String,
    ) -> LocalBoxFuture<'static, Result<String>> {
        async move { stage_agent_binary(distro.as_deref(), BINARY_NAME, &source, &sha256).await }
            .boxed_local()
    }
}

#[cfg(target_os = "windows")]
async fn staged_agent_binary(
    distro: Option<&str>,
    name: &str,
    expected_sha256: &str,
) -> Result<Option<String>> {
    sandbox::staged_agent_binary(distro, name, expected_sha256).await
}

#[cfg(target_os = "windows")]
async fn stage_agent_binary(
    distro: Option<&str>,
    name: &str,
    source_windows_path: &Path,
    expected_sha256: &str,
) -> Result<String> {
    sandbox::stage_agent_binary(distro, name, source_windows_path, expected_sha256).await
}

#[cfg(target_os = "windows")]
async fn wsl_probe(distro: Option<&str>) -> Result<String> {
    use util::command::Stdio;

    // The copy Windows ships, not whatever `wsl.exe` comes first on `PATH`.
    let wsl_exe = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("wsl.exe");
    let mut command = util::command::new_command(&wsl_exe);
    if let Some(distro) = distro {
        command.arg("--distribution").arg(distro);
    }
    command
        .args(["--exec", "sh", "-c", PROBE_SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .context("WSL を起動できませんでした。WSL がインストールされているか確認してください。")?;
    if !output.status.success() {
        bail!(
            "WSL のアーキテクチャを調べられませんでした (終了コード {:?}): {}",
            output.status.code(),
            decode_wsl_output(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `wsl.exe`'s own messages are UTF-16LE; what a Linux program prints is UTF-8.
#[cfg(target_os = "windows")]
fn decode_wsl_output(bytes: &[u8]) -> String {
    let zero_high_bytes = bytes
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|byte| **byte == 0)
        .count();
    let looks_like_utf16 =
        bytes.len() >= 2 && bytes.len().is_multiple_of(2) && zero_high_bytes * 4 >= bytes.len();
    if looks_like_utf16 {
        let units = bytes
            .chunks_exact(2)
            .filter_map(|pair| match pair {
                [low, high] => Some(u16::from_le_bytes([*low, *high])),
                _ => None,
            })
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(not(target_os = "windows"))]
async fn wsl_probe(_distro: Option<&str>) -> Result<String> {
    bail!(WINDOWS_ONLY)
}

#[cfg(not(target_os = "windows"))]
async fn staged_agent_binary(
    _distro: Option<&str>,
    _name: &str,
    _expected_sha256: &str,
) -> Result<Option<String>> {
    bail!(WINDOWS_ONLY)
}

#[cfg(not(target_os = "windows"))]
async fn stage_agent_binary(
    _distro: Option<&str>,
    _name: &str,
    _source_windows_path: &Path,
    _expected_sha256: &str,
) -> Result<String> {
    bail!(WINDOWS_ONLY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{channel::oneshot, future};
    use gpui::TestAppContext;
    use http_client::{
        AllowlistedHttpClient, EgressPolicy, FakeHttpClient, Request, RequestHost, Response,
    };
    use parking_lot::Mutex;
    use sha1::Sha1;
    use std::{cell::Cell, collections::VecDeque, rc::Rc};

    const TEST_RELEASES_URL: &str = "https://downloads.test.invalid/claude-code-releases";
    const TEST_HOST: &str = "downloads.test.invalid";
    const TEST_CDN_URL: &str = "https://cdn.test.invalid/claude.zst";
    const TEST_CDN_HOST: &str = "cdn.test.invalid";
    const ANTHROPIC_RELEASE_KEY: &str =
        include_str!("managed_claude_binary/anthropic-claude-code-release.asc");
    const TEST_RELEASE_KEY: &str =
        include_str!("managed_claude_binary/test_fixtures/test-release-key.asc");
    const TEST_RELEASE_KEY_FINGERPRINT: &str = "1E0709976F81CF4C294621B557BAB549014EDEA7";
    const TEST_OTHER_KEY: &str =
        include_str!("managed_claude_binary/test_fixtures/test-other-key.asc");
    const TEST_OTHER_KEY_FINGERPRINT: &str = "5A543F9F5DF931506A9511082118159F58378B4E";

    const X64_PROBE: &str =
        "zed-probe machine x86_64\nzed-probe cpu-flags  fpu sse2 avx avx2\nzed-probe libc glibc\n";
    const X64_WITHOUT_AVX2_PROBE: &str =
        "zed-probe machine x86_64\nzed-probe cpu-flags  fpu sse2 avx\nzed-probe libc glibc\n";
    const MUSL_PROBE: &str =
        "zed-probe machine x86_64\nzed-probe cpu-flags  avx avx2\nzed-probe libc musl\n";
    const ARM64_PROBE: &str =
        "zed-probe machine aarch64\nzed-probe cpu-flags \nzed-probe libc glibc\n";
    const RISCV64_PROBE: &str =
        "zed-probe machine riscv64\nzed-probe cpu-flags \nzed-probe libc glibc\n";

    const ANTHROPIC_MANIFEST: &[u8] =
        include_bytes!("managed_claude_binary/test_fixtures/anthropic-2.1.270/manifest.json");
    const ANTHROPIC_MANIFEST_SIGNATURE: &[u8] =
        include_bytes!("managed_claude_binary/test_fixtures/anthropic-2.1.270/manifest.json.sig");
    const ANTHROPIC_ZST_MANIFEST: &[u8] =
        include_bytes!("managed_claude_binary/test_fixtures/anthropic-2.1.270/manifest.zst.json");
    const ANTHROPIC_ZST_MANIFEST_SIGNATURE: &[u8] = include_bytes!(
        "managed_claude_binary/test_fixtures/anthropic-2.1.270/manifest.zst.json.sig"
    );
    /// The 1.2.3 manifest signed by the test key with extra hashed subpackets
    /// (`gpg --sig-notation`): a non-critical notation, a critical one, and a
    /// SHA-384 signature.
    const NOTATION_SIGNATURE: &[u8] = include_bytes!(
        "managed_claude_binary/test_fixtures/releases/1.2.3/manifest.json.notation.sig"
    );
    const CRITICAL_NOTATION_SIGNATURE: &[u8] = include_bytes!(
        "managed_claude_binary/test_fixtures/releases/1.2.3/manifest.json.critical.sig"
    );
    const SHA384_SIGNATURE: &[u8] = include_bytes!(
        "managed_claude_binary/test_fixtures/releases/1.2.3/manifest.json.sha384.sig"
    );
    /// Carries a (critical) expiration subpacket of one day.
    const EXPIRING_SIGNATURE: &[u8] = include_bytes!(
        "managed_claude_binary/test_fixtures/releases/1.2.3/manifest.json.expiring.sig"
    );
    /// Its integer has a leading zero byte (2040 bits for a 2048-bit key), so
    /// the RSA check only passes if it is padded back to the key's width.
    const SHORT_SIGNATURE: &[u8] = include_bytes!(
        "managed_claude_binary/test_fixtures/releases/1.2.3/manifest.json.short.sig"
    );

    macro_rules! release_file {
        ($path:literal) => {
            (
                $path,
                include_bytes!(concat!(
                    "managed_claude_binary/test_fixtures/releases/",
                    $path
                ))
                .as_slice(),
            )
        };
    }

    /// Signed with the test key; regenerate with `gpg --detach-sign --armor
    /// --digest-algo SHA256` if a manifest changes.
    const RELEASE_FILES: &[(&str, &[u8])] = &[
        release_file!("1.2.3/manifest.json"),
        release_file!("1.2.3/manifest.json.sig"),
        release_file!("1.2.3/manifest.zst.json"),
        release_file!("1.2.3/manifest.zst.json.sig"),
        release_file!("1.2.3/linux-x64/claude.zst"),
        release_file!("1.2.3/linux-arm64/claude.zst"),
        release_file!("1.2.4/manifest.json"),
        release_file!("1.2.4/manifest.json.sig"),
        release_file!("1.2.4/manifest.zst.json"),
        release_file!("1.2.4/manifest.zst.json.sig"),
        release_file!("1.2.4/linux-x64/claude.zst"),
        release_file!("1.2.4/linux-arm64/claude.zst"),
    ];

    fn release_file(path: &str) -> &'static [u8] {
        RELEASE_FILES
            .iter()
            .find(|(candidate, _)| *candidate == path)
            .map(|(_, contents)| *contents)
            .unwrap_or_else(|| panic!("no fixture {path}"))
    }

    fn release_url(path: &str) -> String {
        format!("{TEST_RELEASES_URL}/{path}")
    }

    fn version(text: &str) -> Version {
        Version::parse(text).expect("valid test version")
    }

    struct PublicKey {
        modulus: Vec<u8>,
        exponent: Vec<u8>,
        fingerprint: [u8; 20],
    }

    /// The primary key of an armored key block, with its version 4 fingerprint
    /// (RFC 4880 §12.2: SHA-1 over 0x99, the two-byte length and the packet).
    fn parse_public_key(armored: &str) -> PublicKey {
        let bytes = dearmor(armored, "PGP PUBLIC KEY BLOCK").expect("armored key");
        let (tag, body, _other_packets) = read_packet(&bytes).expect("key packet");
        assert_eq!(tag, 6, "not a public-key packet");
        let mut reader = Reader::new(body);
        assert_eq!(reader.byte().expect("version"), 4);
        reader.take(4).expect("creation time");
        assert_eq!(reader.byte().expect("algorithm"), RSA_ALGORITHM);
        let modulus = read_mpi(&mut reader).expect("modulus").to_vec();
        let exponent = read_mpi(&mut reader).expect("exponent").to_vec();
        assert!(reader.is_empty());
        let length = u16::try_from(body.len()).expect("key packet length");
        let mut hasher = Sha1::new();
        hasher.update([0x99]);
        hasher.update(length.to_be_bytes());
        hasher.update(body);
        PublicKey {
            modulus,
            exponent,
            fingerprint: hasher.finalize().into(),
        }
    }

    fn test_source(armored_key: &str, fingerprint: &str) -> ReleaseSource {
        let key = parse_public_key(armored_key);
        assert_eq!(hex::encode_upper(key.fingerprint), fingerprint);
        ReleaseSource {
            releases_url: TEST_RELEASES_URL.to_string(),
            signing_key: RsaPublicKey::new(
                BigUint::from_bytes_be(&key.modulus),
                BigUint::from_bytes_be(&key.exponent),
            )
            .expect("RSA key"),
            signing_key_fingerprint: key.fingerprint,
            pinned_version: version("1.2.3"),
        }
    }

    fn test_key_source() -> ReleaseSource {
        test_source(TEST_RELEASE_KEY, TEST_RELEASE_KEY_FINGERPRINT)
    }

    fn expected_file(manifest_path: &str, platform: LinuxPlatform) -> ExpectedFile {
        let manifest: ReleaseManifest =
            serde_json::from_slice(release_file(manifest_path)).expect("fixture manifest");
        ExpectedFile::from_manifest(&manifest.platforms[platform.key()]).expect("fixture entry")
    }

    fn policy_allowing(hosts: &[String]) -> EgressPolicy {
        let (policy, errors) = EgressPolicy::from_host_strings(hosts);
        assert!(errors.is_empty(), "{errors:?}");
        policy
    }

    /// The test key's signature over the 1.2.3 manifest, as one binary packet.
    fn signature_packet() -> Vec<u8> {
        let armored =
            std::str::from_utf8(release_file("1.2.3/manifest.json.sig")).expect("ascii armor");
        dearmor(armored, "PGP SIGNATURE").expect("armored signature")
    }

    /// Rebuilds `packet` (old format, two-byte length) around an edited body.
    fn with_body(packet: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        assert_eq!(
            packet[0], 0x89,
            "fixture signatures use the old header format"
        );
        let mut body = packet[3..].to_vec();
        edit(&mut body);
        let mut rebuilt = vec![0x89];
        rebuilt.extend_from_slice(&u16::try_from(body.len()).expect("length").to_be_bytes());
        rebuilt.extend(body);
        rebuilt
    }

    /// Offsets inside a signature body: the unhashed length and the integer.
    fn body_layout(body: &[u8]) -> (usize, usize) {
        let hashed = usize::from(u16::from_be_bytes([body[4], body[5]]));
        let unhashed_at = 6 + hashed;
        let unhashed = usize::from(u16::from_be_bytes([
            body[unhashed_at],
            body[unhashed_at + 1],
        ]));
        let integer_at = unhashed_at + 2 + unhashed + 2;
        (unhashed_at, integer_at)
    }

    fn refusal(packet: &[u8]) -> String {
        let manifest = release_file("1.2.3/manifest.json");
        match verify_packet(&test_key_source(), manifest, packet, SystemTime::now()) {
            Ok(()) => panic!("the packet was accepted"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[derive(Default)]
    struct FakeServer {
        files: Mutex<HashMap<String, Vec<u8>>>,
        redirects: Mutex<HashMap<String, String>>,
        stalled: Mutex<Vec<String>>,
        requests: Mutex<Vec<String>>,
    }

    /// A response body that never delivers a byte, like a connection gone quiet.
    struct StalledBody;

    impl futures::AsyncRead for StalledBody {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
    }

    impl FakeServer {
        fn with_releases() -> Arc<Self> {
            let server = Self::default();
            {
                let mut files = server.files.lock();
                for (path, contents) in RELEASE_FILES {
                    files.insert(release_url(path), contents.to_vec());
                }
                files.insert(release_url("stable"), b"1.2.4\n".to_vec());
                files.insert(release_url("latest"), b"1.2.4\n".to_vec());
            }
            Arc::new(server)
        }

        fn set(&self, url: String, contents: Vec<u8>) {
            self.files.lock().insert(url, contents);
        }

        fn get(&self, url: &str) -> Vec<u8> {
            self.files.lock().get(url).cloned().expect("served file")
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().clone()
        }

        fn respond(&self, request: Request<AsyncBody>) -> Result<Response<AsyncBody>> {
            let url = request.uri().to_string();
            self.requests.lock().push(url.clone());
            if let Some(target) = self.redirects.lock().get(&url) {
                return Ok(Response::builder()
                    .status(302)
                    .header("location", target.as_str())
                    .body(AsyncBody::default())?);
            }
            if self.stalled.lock().contains(&url) {
                return Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from_reader(StalledBody))?);
            }
            Ok(match self.files.lock().get(&url) {
                Some(contents) => Response::builder()
                    .status(200)
                    .body(AsyncBody::from(contents.clone()))?,
                None => Response::builder().status(404).body(AsyncBody::default())?,
            })
        }
    }

    struct FakeWsl {
        probe_output: String,
        staged: Mutex<HashMap<String, String>>,
        stage_count: Mutex<usize>,
    }

    impl FakeWsl {
        fn stage_count(&self) -> usize {
            *self.stage_count.lock()
        }

        fn forget_staged(&self) {
            self.staged.lock().clear();
        }
    }

    impl WslAgentHost for FakeWsl {
        fn probe(&self, _distro: Option<String>) -> LocalBoxFuture<'static, Result<String>> {
            future::ready(Ok(self.probe_output.clone())).boxed_local()
        }

        fn staged(
            &self,
            _distro: Option<String>,
            sha256: String,
        ) -> LocalBoxFuture<'static, Result<Option<String>>> {
            future::ready(Ok(self.staged.lock().get(&sha256).cloned())).boxed_local()
        }

        fn stage(
            &self,
            _distro: Option<String>,
            source: PathBuf,
            sha256: String,
        ) -> LocalBoxFuture<'static, Result<String>> {
            let result = std::fs::read(&source)
                .map_err(anyhow::Error::from)
                .and_then(|contents| {
                    // What the real helper checks before it copies anything.
                    ensure!(
                        format!("{:x}", Sha256::digest(&contents)) == sha256,
                        "staged file does not hash to {sha256}"
                    );
                    let path = staged_path(&sha256);
                    self.staged.lock().insert(sha256, path.clone());
                    *self.stage_count.lock() += 1;
                    Ok(path)
                });
            future::ready(result).boxed_local()
        }
    }

    struct FakeUi {
        answers: VecDeque<bool>,
        prompts: Vec<(ConsentRequest, Vec<String>)>,
        policy: EgressPolicyHandle,
        allowed: Vec<String>,
        added: Vec<String>,
    }

    impl InstallerUi for FakeUi {
        fn missing_hosts(&mut self, hosts: &[String]) -> Result<Vec<String>> {
            Ok(hosts
                .iter()
                .filter(|host| {
                    !RequestHost::parse(host).is_some_and(|host| self.policy.allows(&host))
                })
                .cloned()
                .collect())
        }

        fn confirm(
            &mut self,
            request: &ConsentRequest,
            _hosts: &[String],
            missing: &[String],
        ) -> LocalBoxFuture<'static, Result<bool>> {
            self.prompts.push((request.clone(), missing.to_vec()));
            let answer = self
                .answers
                .pop_front()
                .with_context(|| format!("unexpected prompt {request:?}"));
            future::ready(answer).boxed_local()
        }

        fn add_allowed_hosts(&mut self, hosts: Vec<String>) -> LocalBoxFuture<'static, Result<()>> {
            self.added.extend(hosts.iter().cloned());
            self.allowed.extend(hosts);
            self.policy.set(policy_allowing(&self.allowed));
            future::ready(Ok(())).boxed_local()
        }
    }

    struct Harness {
        installer: Installer,
        server: Arc<FakeServer>,
        wsl: Arc<FakeWsl>,
        policy: EgressPolicyHandle,
        allowed: Vec<String>,
        progress: Arc<Mutex<Vec<ManagedClaudeProgress>>>,
        storage: tempfile::TempDir,
    }

    impl Harness {
        fn new(cx: &TestAppContext, probe_output: &str, allowed_hosts: &[&str]) -> Self {
            let server = FakeServer::with_releases();
            let fake_http = FakeHttpClient::create({
                let server = server.clone();
                move |request| {
                    let response = server.respond(request);
                    async move { response }
                }
            });
            let allowed = allowed_hosts
                .iter()
                .map(|host| host.to_string())
                .collect::<Vec<_>>();
            let policy = EgressPolicyHandle::new(policy_allowing(&allowed));
            let wsl = Arc::new(FakeWsl {
                probe_output: probe_output.to_string(),
                staged: Mutex::default(),
                stage_count: Mutex::default(),
            });
            let storage = tempfile::tempdir().expect("temporary directory");
            let installer = Installer {
                // The real gate, so that consent and denial are exercised
                // against what production requests go through.
                http: Arc::new(AllowlistedHttpClient::new(fake_http, policy.clone())),
                wsl: wsl.clone(),
                executor: cx.executor(),
                storage_dir: storage.path().join("claude-code"),
                source: Arc::new(test_key_source()),
                state_lock: Arc::default(),
            };
            Self {
                installer,
                server,
                wsl,
                policy,
                allowed,
                progress: Arc::default(),
                storage,
            }
        }

        fn ui(&self, answers: &[bool]) -> FakeUi {
            FakeUi {
                answers: answers.iter().copied().collect(),
                prompts: Vec::new(),
                policy: self.policy.clone(),
                allowed: self.allowed.clone(),
                added: Vec::new(),
            }
        }

        async fn run(&self, goal: Goal, ui: &mut FakeUi) -> Result<ManagedClaudeBinary> {
            let progress = self.progress.clone();
            self.installer
                .run(
                    None,
                    goal,
                    ui,
                    Arc::new(move |update| progress.lock().push(update)),
                )
                .await
        }

        fn state(&self) -> InstallerState {
            std::fs::read(self.installer.state_path())
                .map(|contents| serde_json::from_slice(&contents).expect("state file"))
                .unwrap_or_default()
        }

        fn write_state(&self, state: &InstallerState) {
            let path = self.installer.state_path();
            std::fs::create_dir_all(path.parent().expect("state directory")).expect("create");
            std::fs::write(&path, serde_json::to_vec(state).expect("serialize")).expect("write");
        }

        fn files_left_in_downloads(&self) -> Vec<PathBuf> {
            let mut pending = vec![self.storage.path().join("claude-code").join("downloads")];
            let mut files = Vec::new();
            while let Some(directory) = pending.pop() {
                let Ok(entries) = std::fs::read_dir(&directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        pending.push(path);
                    } else {
                        files.push(path);
                    }
                }
            }
            files
        }
    }

    fn staged_path(sha256: &str) -> String {
        format!("/home/test/.cache/zed-jp-enhance/agents/{sha256}/claude")
    }

    fn release_requests(version: &str, platform: &str) -> Vec<String> {
        vec![
            release_url(&format!("{version}/manifest.json")),
            release_url(&format!("{version}/manifest.json.sig")),
            release_url(&format!("{version}/manifest.zst.json")),
            release_url(&format!("{version}/manifest.zst.json.sig")),
            release_url(&format!("{version}/{platform}/claude.zst")),
        ]
    }

    #[test]
    fn test_embedded_release_key_matches_the_published_key() {
        let published = parse_public_key(ANTHROPIC_RELEASE_KEY);
        assert_eq!(
            hex::encode_upper(published.fingerprint),
            RELEASE_SIGNING_KEY_FINGERPRINT
        );
        assert_eq!(
            BASE64
                .decode(RELEASE_SIGNING_KEY_MODULUS)
                .expect("base64 modulus"),
            published.modulus
        );
        assert_eq!(RELEASE_SIGNING_KEY_EXPONENT.as_slice(), published.exponent);
        let source = ReleaseSource::anthropic().expect("the embedded key loads");
        assert_eq!(source.signing_key.size(), 512);
        assert_eq!(source.signing_key_fingerprint, published.fingerprint);
    }

    #[test]
    fn test_anthropic_manifests_verify_against_the_embedded_key() {
        let source = ReleaseSource::anthropic().expect("the embedded key loads");
        let version = version("2.1.270");
        let binary = verified_manifest_entry(
            &source,
            version,
            LinuxPlatform::X64,
            ANTHROPIC_MANIFEST,
            ANTHROPIC_MANIFEST_SIGNATURE,
            BINARY_NAME,
            SystemTime::now(),
        )
        .expect("the real 2.1.270 manifest verifies");
        assert_eq!(
            binary,
            ExpectedFile {
                sha256: "3a624a5a7cd79bbad4d32bd7db36f1197ecf458bc5bf1e2aed81834a01ad3ef0"
                    .to_string(),
                size: 223_981_040,
            }
        );
        let compressed = verified_manifest_entry(
            &source,
            version,
            LinuxPlatform::X64,
            ANTHROPIC_ZST_MANIFEST,
            ANTHROPIC_ZST_MANIFEST_SIGNATURE,
            COMPRESSED_BINARY_NAME,
            SystemTime::now(),
        )
        .expect("the real 2.1.270 zst manifest verifies");
        assert_eq!(
            compressed,
            ExpectedFile {
                sha256: "59c056cd321c131eb211f6e90f4b9812dd24820357169f2ec9ca6a4b51d89d9c"
                    .to_string(),
                size: 78_038_652,
            }
        );
        let arm64 = verified_manifest_entry(
            &source,
            version,
            LinuxPlatform::Arm64,
            ANTHROPIC_MANIFEST,
            ANTHROPIC_MANIFEST_SIGNATURE,
            BINARY_NAME,
            SystemTime::now(),
        )
        .expect("the real 2.1.270 manifest lists arm64");
        assert_eq!(
            arm64.sha256,
            "7bf9f33acc124df9abccf6f2366397a82a740378d535fa12d426fa77fdbc9946"
        );

        let error = verified_manifest_entry(
            &source,
            Version::parse("2.1.271").expect("valid"),
            LinuxPlatform::X64,
            ANTHROPIC_MANIFEST,
            ANTHROPIC_MANIFEST_SIGNATURE,
            BINARY_NAME,
            SystemTime::now(),
        )
        .expect_err("a manifest for another version is refused");
        assert!(error.to_string().contains("一致しません"), "{error:#}");
    }

    #[test]
    fn test_signatures_that_do_not_match_are_refused() {
        let manifest = release_file("1.2.3/manifest.json");
        let signature = release_file("1.2.3/manifest.json.sig");
        let verify = |source: &ReleaseSource, manifest: &[u8], signature: &[u8]| {
            verified_manifest_entry(
                source,
                version("1.2.3"),
                LinuxPlatform::X64,
                manifest,
                signature,
                BINARY_NAME,
                SystemTime::now(),
            )
        };
        verify(&test_key_source(), manifest, signature).expect("the untouched fixture verifies");
        verify(&test_key_source(), manifest, SHA384_SIGNATURE)
            .expect("a SHA-384 signature verifies");

        let tampered = String::from_utf8(manifest.to_vec())
            .expect("utf-8 manifest")
            .replacen("\"version\": \"1.2.3\"", "\"version\": \"1.2.3\" ", 1);
        let error = verify(&test_key_source(), tampered.as_bytes(), signature)
            .expect_err("a tampered manifest is refused");
        assert_eq!(error.to_string(), SIGNATURE_REFUSED);

        let error = verify(
            &test_key_source(),
            manifest,
            release_file("1.2.3/manifest.zst.json.sig"),
        )
        .expect_err("a signature over other content is refused");
        assert_eq!(error.to_string(), SIGNATURE_REFUSED);

        let other_key = test_source(TEST_OTHER_KEY, TEST_OTHER_KEY_FINGERPRINT);
        let error =
            verify(&other_key, manifest, signature).expect_err("another key's pin is refused");
        assert!(
            format!("{error:#}").contains("not by the pinned key"),
            "{error:#}"
        );

        let error = verify(
            &test_key_source(),
            ANTHROPIC_MANIFEST,
            ANTHROPIC_MANIFEST_SIGNATURE,
        )
        .expect_err("a manifest signed by a key that is not the pinned one is refused");
        assert_eq!(error.to_string(), SIGNATURE_REFUSED);

        // The pin matches what the signature names, but the RSA key is another
        // one: only the RSA check itself can catch this.
        let wrong_rsa_key = ReleaseSource {
            signing_key: other_key.signing_key,
            ..test_key_source()
        };
        let error = verify(&wrong_rsa_key, manifest, signature)
            .expect_err("a signature by another RSA key is refused");
        assert!(
            format!("{error:#}").contains("does not verify"),
            "{error:#}"
        );

        let error = verify(&test_key_source(), manifest, b"not a signature")
            .expect_err("garbage instead of a signature is refused");
        assert_eq!(error.to_string(), SIGNATURE_REFUSED);
    }

    #[test]
    fn test_armor_is_read_strictly() {
        let manifest = release_file("1.2.3/manifest.json");
        let armored =
            std::str::from_utf8(release_file("1.2.3/manifest.json.sig")).expect("ascii armor");
        let source = test_key_source();
        let verify_signature = |source: &ReleaseSource, manifest: &[u8], armored: &[u8]| {
            super::verify_signature(source, manifest, armored, SystemTime::now())
        };

        let without_checksum = armored
            .lines()
            .filter(|line| !line.starts_with('='))
            .collect::<Vec<_>>()
            .join("\n");
        verify_signature(&source, manifest, without_checksum.as_bytes())
            .expect("the checksum line is optional");

        let checksum_line = armored
            .lines()
            .find(|line| line.starts_with('='))
            .expect("checksum line");
        let wrong_checksum = armored.replace(checksum_line, "=AAAA");
        let error = verify_signature(&source, manifest, wrong_checksum.as_bytes())
            .expect_err("a wrong checksum is refused");
        assert!(format!("{error:#}").contains("checksum"), "{error:#}");

        let two_blocks = format!("{armored}{armored}");
        assert!(verify_signature(&source, manifest, two_blocks.as_bytes()).is_err());
        let with_trailer = format!("{armored}\nextra\n");
        assert!(verify_signature(&source, manifest, with_trailer.as_bytes()).is_err());
        let wrong_label = armored.replace("PGP SIGNATURE", "PGP MESSAGE");
        assert!(verify_signature(&source, manifest, wrong_label.as_bytes()).is_err());
    }

    #[test]
    fn test_packets_parse_in_both_length_formats() {
        let manifest = release_file("1.2.3/manifest.json");
        let packet = signature_packet();
        let body = &packet[3..];
        let verify_packet = |packet: &[u8]| {
            super::verify_packet(&test_key_source(), manifest, packet, SystemTime::now())
        };
        verify_packet(&packet).expect("old format");

        let length = body.len() - 192;
        let mut two_octet = vec![0xc2, u8::try_from((length >> 8) + 192).expect("octet")];
        two_octet.push(u8::try_from(length & 0xff).expect("octet"));
        two_octet.extend_from_slice(body);
        verify_packet(&two_octet).expect("new format, two octets");

        let mut five_octet = vec![0xc2, 0xff];
        five_octet.extend_from_slice(&u32::try_from(body.len()).expect("length").to_be_bytes());
        five_octet.extend_from_slice(body);
        verify_packet(&five_octet).expect("new format, five octets");
    }

    #[test]
    fn test_truncated_and_mislabeled_packets_are_refused() {
        let packet = signature_packet();
        for length in [0, 1, 2, 3, 10, packet.len() / 2, packet.len() - 1] {
            let error = refusal(&packet[..length]);
            assert!(error.contains("truncated"), "{length}: {error}");
        }

        let mut too_long = packet.clone();
        too_long[2] = too_long[2].wrapping_add(1);
        assert!(refusal(&too_long).contains("truncated"));

        let mut indeterminate = packet.clone();
        indeterminate[0] = 0x8b;
        assert!(refusal(&indeterminate).contains("indeterminate"));

        let partial = [0xc2, 0xe1, 0x00, 0x00];
        assert!(refusal(&partial).contains("partial"));

        let mut oversized = vec![0xc2, 0xff];
        oversized.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        assert!(refusal(&oversized).contains("out of range"));

        let mut public_key_tag = packet;
        public_key_tag[0] = 0x99;
        assert!(refusal(&public_key_tag).contains("tag 6"));
    }

    #[test]
    fn test_bogus_inner_lengths_are_refused() {
        let packet = signature_packet();

        let hashed_too_long = with_body(&packet, |body| {
            body[4] = 0xff;
            body[5] = 0xff;
        });
        assert!(refusal(&hashed_too_long).contains("truncated"));

        let unhashed_too_long = with_body(&packet, |body| {
            let (unhashed_at, _) = body_layout(body);
            body[unhashed_at] = 0xff;
        });
        assert!(refusal(&unhashed_too_long).contains("truncated"));

        let subpacket_too_long = with_body(&packet, |body| body[6] = 190);
        assert!(refusal(&subpacket_too_long).contains("truncated"));

        for adjust in [1i32, -1, 8] {
            let wrong_bit_count = with_body(&packet, |body| {
                let (_, integer_at) = body_layout(body);
                let bits = i32::from(u16::from_be_bytes([body[integer_at], body[integer_at + 1]]));
                let bits = u16::try_from(bits + adjust).expect("bit count");
                body[integer_at..integer_at + 2].copy_from_slice(&bits.to_be_bytes());
            });
            let error = refusal(&wrong_bit_count);
            assert!(
                error.contains("bits") || error.contains("truncated") || error.contains("follow"),
                "{adjust}: {error}"
            );
        }

        let trailing_inside = with_body(&packet, |body| body.push(0));
        assert!(refusal(&trailing_inside).contains("follow the signature value"));
    }

    #[test]
    fn test_only_version_4_binary_rsa_sha2_signatures_are_accepted() {
        let packet = signature_packet();
        let edited = |offset: usize, value: u8| with_body(&packet, |body| body[offset] = value);

        for signature_version in [3, 5, 6] {
            assert!(refusal(&edited(0, signature_version)).contains("signature version"));
        }
        assert!(refusal(&edited(1, 0x01)).contains("signature type"));
        assert!(refusal(&edited(2, 17)).contains("not RSA"));
        for hash in [1, 2, 3, 11] {
            assert!(refusal(&edited(3, hash)).contains("hash algorithm"));
        }

        let quick_check_off = with_body(&packet, |body| {
            let (_, integer_at) = body_layout(body);
            body[integer_at - 2] ^= 0xff;
        });
        assert!(refusal(&quick_check_off).contains("quick check"));
    }

    #[test]
    fn test_unknown_critical_subpackets_are_refused() {
        let manifest = release_file("1.2.3/manifest.json");
        let verify = |signature: &[u8]| {
            verify_signature(&test_key_source(), manifest, signature, SystemTime::now())
        };
        verify(NOTATION_SIGNATURE).expect("a non-critical notation is ignored");
        let error =
            verify(CRITICAL_NOTATION_SIGNATURE).expect_err("a critical notation is not understood");
        assert!(
            format!("{error:#}").contains("critical signature subpacket 20"),
            "{error:#}"
        );
    }

    #[test]
    fn test_two_signatures_are_refused() {
        let packet = signature_packet();
        let mut doubled = packet.clone();
        doubled.extend_from_slice(&packet);
        assert!(refusal(&doubled).contains("follow the signature packet"));
    }

    fn armored_packet(armored: &[u8]) -> Vec<u8> {
        let armored = std::str::from_utf8(armored).expect("ascii armor");
        dearmor(armored, "PGP SIGNATURE").expect("armored signature")
    }

    fn signature_creation_time(packet: &[u8]) -> u64 {
        let (_, body, _) = read_packet(packet).expect("packet");
        let hashed = usize::from(u16::from_be_bytes([body[4], body[5]]));
        read_subpackets(&body[6..6 + hashed])
            .expect("subpackets")
            .iter()
            .find(|subpacket| subpacket.kind == SUBPACKET_CREATION_TIME)
            .map(|subpacket| u64::from(subpacket_time(subpacket.data).expect("time")))
            .expect("creation time")
    }

    #[test]
    fn test_a_short_signature_integer_is_padded_to_the_key_width() {
        let packet = armored_packet(SHORT_SIGNATURE);
        let source = test_key_source();
        let signature =
            parse_signature(&packet, &source.signing_key_fingerprint, 0).expect("parses");
        assert!(signature.value.len() < source.signing_key.size());
        verify_packet(
            &source,
            release_file("1.2.3/manifest.json"),
            &packet,
            SystemTime::now(),
        )
        .expect("verifies once padded");
    }

    #[test]
    fn test_the_issuer_key_id_counts_only_when_it_is_signed() {
        let packet = signature_packet();
        let other_key = parse_public_key(TEST_OTHER_KEY);
        let other_key_id = &other_key.fingerprint[12..];

        let hashed_other_issuer = with_body(&packet, |body| {
            let hashed = u16::from_be_bytes([body[4], body[5]]);
            let mut subpacket = vec![9, SUBPACKET_ISSUER_KEY_ID];
            subpacket.extend_from_slice(other_key_id);
            body.splice(6..6, subpacket);
            body[4..6].copy_from_slice(&(hashed + 10).to_be_bytes());
        });
        let error = refusal(&hashed_other_issuer);
        assert!(error.contains("key ID"), "{error}");

        let unhashed_other_issuer = with_body(&packet, |body| {
            let (unhashed_at, _) = body_layout(body);
            let area = unhashed_at + 2;
            assert_eq!(body[area..area + 2], [9, SUBPACKET_ISSUER_KEY_ID]);
            body[area + 2..area + 10].copy_from_slice(other_key_id);
        });
        verify_packet(
            &test_key_source(),
            release_file("1.2.3/manifest.json"),
            &unhashed_other_issuer,
            SystemTime::now(),
        )
        .expect("nothing in the unhashed area is signed, so it is ignored");
    }

    #[test]
    fn test_the_zst_manifest_does_not_pass_for_the_binary_manifest() {
        let error = verified_manifest_entry(
            &test_key_source(),
            version("1.2.3"),
            LinuxPlatform::X64,
            release_file("1.2.3/manifest.zst.json"),
            release_file("1.2.3/manifest.zst.json.sig"),
            BINARY_NAME,
            SystemTime::now(),
        )
        .expect_err("claude.zst is not claude");
        assert!(error.to_string().contains("ファイル名"), "{error:#}");
    }

    #[test]
    fn test_an_expired_signature_is_refused() {
        let manifest = release_file("1.2.3/manifest.json");
        let packet = armored_packet(EXPIRING_SIGNATURE);
        let created = signature_creation_time(&packet);
        let at = |seconds: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(created + seconds);
        verify_packet(&test_key_source(), manifest, &packet, at(60 * 60))
            .expect("valid within its day");
        let error = verify_packet(&test_key_source(), manifest, &packet, at(2 * 24 * 60 * 60))
            .expect_err("expired");
        assert!(format!("{error:#}").contains("expired"), "{error:#}");
    }

    #[test]
    fn test_decompressed_binary_must_match_its_manifest() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let compressed = directory.path().join("claude.zst");
        std::fs::write(&compressed, release_file("1.2.3/linux-x64/claude.zst"))
            .expect("write fixture");
        let destination = directory.path().join("claude");
        let expected = expected_file("1.2.3/manifest.json", LinuxPlatform::X64);

        decompress_verified(&compressed, &destination, &expected).expect("matches");
        let contents = std::fs::read(&destination).expect("decompressed");
        assert_eq!(format!("{:x}", Sha256::digest(&contents)), expected.sha256);

        let wrong_hash = ExpectedFile {
            sha256: "0".repeat(64),
            size: expected.size,
        };
        let error =
            decompress_verified(&compressed, &destination, &wrong_hash).expect_err("wrong hash");
        assert_eq!(error.to_string(), CHECKSUM_REFUSED);

        for size in [expected.size - 1, expected.size + 1] {
            let wrong_size = ExpectedFile {
                size,
                ..expected.clone()
            };
            let error = decompress_verified(&compressed, &destination, &wrong_size)
                .expect_err("wrong size");
            assert_eq!(error.to_string(), SIZE_REFUSED);
        }
    }

    #[test]
    fn test_versions_parse_only_in_canonical_form() {
        assert_eq!(
            Version::parse("2.1.270"),
            Some(Version {
                major: 2,
                minor: 1,
                patch: 270
            })
        );
        for text in [
            "",
            "2.1",
            "2.1.270.1",
            "2.1.270-beta",
            "v2.1.270",
            "02.1.270",
            "2.1.x",
            "../2.1.270",
            "2..270",
            " 2.1.270",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
        assert!(version("2.1.270") > version("2.1.236"));
        assert!(version("2.10.0") > version("2.9.99"));
    }

    #[test]
    fn test_probe_output_decides_the_platform() {
        assert_eq!(
            platform_from_probe(X64_PROBE).expect("x64 with AVX2"),
            LinuxPlatform::X64
        );
        assert_eq!(
            platform_from_probe(ARM64_PROBE).expect("arm64"),
            LinuxPlatform::Arm64
        );
        // Lines without the marker, such as WSL's own messages, are ignored.
        let noisy = format!("machine riscv64\nlibc musl\n{X64_PROBE}警告: wsl\n");
        assert_eq!(
            platform_from_probe(&noisy).expect("noise is ignored"),
            LinuxPlatform::X64
        );
        let unknown_libc = X64_PROBE.replace("libc glibc", "libc unknown");
        assert_eq!(
            platform_from_probe(&unknown_libc).expect("an unknown libc is let through"),
            LinuxPlatform::X64
        );

        let error = platform_from_probe(X64_WITHOUT_AVX2_PROBE).expect_err("no AVX2");
        assert_eq!(error.to_string(), NO_AVX2);
        let error = platform_from_probe(MUSL_PROBE).expect_err("musl");
        assert_eq!(error.to_string(), NOT_GLIBC);
        let error = platform_from_probe("x86_64\nflags : avx2\n").expect_err("no marker");
        assert_eq!(error.to_string(), PROBE_UNREADABLE);
        for probe in [
            RISCV64_PROBE,
            "zed-probe machine armv7l\n",
            "zed-probe machine i686\n",
        ] {
            assert!(platform_from_probe(probe).is_err(), "{probe:?}");
        }
    }

    #[test]
    fn test_stale_downloads_are_swept() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let downloads = directory.path().join("downloads");
        let interrupted = downloads.join("claude-interrupted");
        std::fs::create_dir_all(&interrupted).expect("create");
        std::fs::write(interrupted.join("claude.zst.partial"), b"partial").expect("write");
        std::fs::write(downloads.join("stray"), b"stray").expect("write");

        sweep_stale_downloads(&downloads, SystemTime::now());
        assert!(interrupted.exists() && downloads.join("stray").exists());

        sweep_stale_downloads(&downloads, SystemTime::now() + STALE_DOWNLOAD_AGE * 2);
        assert_eq!(std::fs::read_dir(&downloads).expect("list").count(), 0);

        sweep_stale_downloads(&directory.path().join("missing"), SystemTime::now());
    }

    #[test]
    fn test_consent_prompts_use_the_install_consent_wording() {
        let hosts = vec!["downloads.claude.ai".to_string()];
        let install = ConsentRequest::Install {
            version: version("2.1.270"),
        };
        let prompt = consent_prompt(&install, &hosts, &hosts);
        assert_eq!(prompt.message, "Linux 版 Claude Code v2.1.270 を取得します");
        assert_eq!(prompt.confirm, "許可リストに追加してインストール");
        assert!(
            prompt
                .detail
                .ends_with("許可が必要なホスト: downloads.claude.ai (未登録)"),
            "{}",
            prompt.detail
        );
        let prompt = consent_prompt(&install, &hosts, &[]);
        assert_eq!(prompt.confirm, "了承してインストール");
        assert!(prompt.detail.ends_with("(登録済み)"), "{}", prompt.detail);

        let update = ConsentRequest::Update {
            from: version("2.1.270"),
            to: version("2.1.280"),
        };
        let prompt = consent_prompt(&update, &hosts, &[]);
        assert_eq!(prompt.message, "Claude Code を v2.1.280 に更新します");
        assert!(
            prompt.detail.starts_with("現在の版: v2.1.270"),
            "{}",
            prompt.detail
        );
        assert_eq!(prompt.confirm, "了承して更新");
        assert_eq!(
            consent_prompt(&update, &hosts, &hosts).confirm,
            "許可リストに追加して更新"
        );
    }

    #[gpui::test]
    async fn test_concurrent_ensures_share_one_install(cx: &mut TestAppContext) {
        let binary = ManagedClaudeBinary {
            version: "1.2.3".to_string(),
            sha256: "0".repeat(64),
            path_in_wsl: staged_path(&"0".repeat(64)),
        };
        let starts = Rc::new(Cell::new(0));
        let (finish_tx, finish_rx) = oneshot::channel::<()>();
        let first = cx.update(|cx| {
            join_or_start(None, cx, {
                let starts = starts.clone();
                let binary = binary.clone();
                move |cx| {
                    starts.set(starts.get() + 1);
                    cx.spawn(async move |_| {
                        finish_rx.await.expect("the test finishes the install");
                        Ok(binary)
                    })
                }
            })
        });
        let second = cx.update(|cx| {
            join_or_start(None, cx, |_| unreachable!("the running install is joined"))
        });
        let other_distro = cx.update(|cx| {
            join_or_start(Some("Debian".to_string()), cx, |_| {
                Task::ready(Err(anyhow!("inner detail").context(DOWNLOAD_DECLINED)))
            })
        });
        finish_tx.send(()).expect("install waiting");
        assert_eq!(first.await.expect("first"), binary);
        assert_eq!(second.await.expect("second"), binary);
        assert_eq!(starts.get(), 1);

        let error = other_distro.await.expect_err("failed install");
        assert_eq!(error.to_string(), DOWNLOAD_DECLINED);
        assert_eq!(
            format!("{error:#}"),
            format!("{DOWNLOAD_DECLINED}: inner detail")
        );

        let third = cx.update(|cx| {
            join_or_start(None, cx, {
                let starts = starts.clone();
                let binary = binary.clone();
                move |_| {
                    starts.set(starts.get() + 1);
                    Task::ready(Ok(binary))
                }
            })
        });
        third.await.expect("third");
        assert_eq!(starts.get(), 2, "a finished install is not joined again");
    }

    #[gpui::test]
    async fn test_first_install_downloads_verifies_and_stages(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let mut ui = harness.ui(&[true]);
        let binary = harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("first install");

        let expected = expected_file("1.2.3/manifest.json", LinuxPlatform::X64);
        assert_eq!(
            binary,
            ManagedClaudeBinary {
                version: "1.2.3".to_string(),
                sha256: expected.sha256.clone(),
                path_in_wsl: staged_path(&expected.sha256),
            }
        );
        assert_eq!(
            ui.prompts,
            vec![(
                ConsentRequest::Install {
                    version: version("1.2.3")
                },
                Vec::new()
            )]
        );
        assert_eq!(
            harness.server.requests(),
            release_requests("1.2.3", "linux-x64")
        );
        assert_eq!(harness.wsl.stage_count(), 1);
        assert!(
            harness.files_left_in_downloads().is_empty(),
            "{:?}",
            harness.files_left_in_downloads()
        );

        let state = harness.state();
        assert_eq!(state.installed_version.as_deref(), Some("1.2.3"));
        assert!(state.consented_versions.contains("1.2.3"));
        assert_eq!(
            state.verified_binaries.get("1.2.3/linux-x64"),
            Some(&VerifiedBinary {
                sha256: expected.sha256,
                signing_key: TEST_RELEASE_KEY_FINGERPRINT.to_string(),
            })
        );

        let compressed = expected_file("1.2.3/manifest.zst.json", LinuxPlatform::X64);
        let progress = harness.progress.lock().clone();
        assert!(progress.contains(&ManagedClaudeProgress::Verifying));
        assert!(progress.contains(&ManagedClaudeProgress::Downloading {
            received: compressed.size,
            total: compressed.size,
        }));
        assert!(progress.contains(&ManagedClaudeProgress::Extracting));
        assert!(progress.contains(&ManagedClaudeProgress::Staging));
    }

    #[gpui::test]
    async fn test_an_arm64_distro_gets_the_arm64_build(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, ARM64_PROBE, &[TEST_HOST]);
        let binary = harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("arm64 install");
        let expected = expected_file("1.2.3/manifest.json", LinuxPlatform::Arm64);
        assert_eq!(binary.sha256, expected.sha256);
        assert_eq!(
            harness.server.requests(),
            release_requests("1.2.3", "linux-arm64")
        );
    }

    #[gpui::test]
    async fn test_an_unsupported_cpu_is_refused_before_asking_anything(cx: &mut TestAppContext) {
        for (probe, expected) in [
            (RISCV64_PROBE, "riscv64"),
            (X64_WITHOUT_AVX2_PROBE, "AVX2"),
            (MUSL_PROBE, "musl"),
        ] {
            let harness = Harness::new(cx, probe, &[TEST_HOST]);
            let mut ui = harness.ui(&[]);
            let error = harness
                .run(Goal::Ensure, &mut ui)
                .await
                .expect_err("refused");
            assert!(error.to_string().contains(expected), "{error:#}");
            assert!(ui.prompts.is_empty());
            assert!(harness.server.requests().is_empty());
            assert_eq!(harness.wsl.stage_count(), 0);
        }
    }

    #[gpui::test]
    async fn test_declining_makes_no_request(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let mut ui = harness.ui(&[false]);
        let error = harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect_err("declined");
        assert_eq!(error.to_string(), DOWNLOAD_DECLINED);
        assert_eq!(ui.prompts.len(), 1);
        assert!(harness.server.requests().is_empty());
        assert_eq!(harness.wsl.stage_count(), 0);
        assert!(harness.state().consented_versions.is_empty());
    }

    #[gpui::test]
    async fn test_a_staged_binary_is_reused_without_asking_or_downloading(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let first = harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        let requests = harness.server.requests();

        let mut ui = harness.ui(&[]);
        let second = harness.run(Goal::Ensure, &mut ui).await.expect("reuse");
        assert_eq!(second, first);
        assert!(ui.prompts.is_empty());
        assert_eq!(harness.server.requests(), requests);
        assert_eq!(harness.wsl.stage_count(), 1);
    }

    #[gpui::test]
    async fn test_consent_is_asked_once_per_version(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        harness.wsl.forget_staged();

        let mut ui = harness.ui(&[]);
        harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("staged again without asking");
        assert!(ui.prompts.is_empty());
        assert_eq!(harness.wsl.stage_count(), 2);
        let mut expected = release_requests("1.2.3", "linux-x64");
        expected.extend(release_requests("1.2.3", "linux-x64"));
        assert_eq!(harness.server.requests(), expected);
    }

    #[gpui::test]
    async fn test_a_host_missing_from_the_allowlist_goes_through_the_allowlist_consent(
        cx: &mut TestAppContext,
    ) {
        let harness = Harness::new(cx, X64_PROBE, &[]);
        let mut ui = harness.ui(&[true]);
        harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("install after adding the host");
        assert_eq!(
            ui.prompts,
            vec![(
                ConsentRequest::Install {
                    version: version("1.2.3")
                },
                vec![TEST_HOST.to_string()]
            )]
        );
        assert_eq!(ui.added, vec![TEST_HOST.to_string()]);
    }

    #[gpui::test]
    async fn test_a_redirect_is_refused_without_offering_its_host(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let download_url = release_url("1.2.3/linux-x64/claude.zst");
        let contents = harness.server.get(&download_url);
        harness.server.set(TEST_CDN_URL.to_string(), contents);
        harness
            .server
            .redirects
            .lock()
            .insert(download_url, TEST_CDN_URL.to_string());

        let mut ui = harness.ui(&[true]);
        let error = harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect_err("the redirect is refused");
        assert_eq!(error.to_string(), REDIRECT_REFUSED);
        assert!(format!("{error:#}").contains(TEST_CDN_HOST), "{error:#}");
        assert_eq!(ui.prompts.len(), 1, "{:?}", ui.prompts);
        assert!(ui.added.is_empty());
        assert!(
            !harness
                .server
                .requests()
                .contains(&TEST_CDN_URL.to_string())
        );
        assert_eq!(harness.wsl.stage_count(), 0);
        assert!(harness.files_left_in_downloads().is_empty());
    }

    #[gpui::test]
    async fn test_a_stalled_download_is_given_up(cx: &mut TestAppContext) {
        let harness = Rc::new(Harness::new(cx, X64_PROBE, &[TEST_HOST]));
        harness
            .server
            .stalled
            .lock()
            .push(release_url("1.2.3/linux-x64/claude.zst"));
        let run = cx.foreground_executor().spawn({
            let harness = harness.clone();
            async move { harness.run(Goal::Ensure, &mut harness.ui(&[true])).await }
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(IDLE_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();
        let error = run.await.expect_err("the stalled download is given up");
        assert_eq!(error.to_string(), STALLED);
        assert_eq!(harness.wsl.stage_count(), 0);
        assert!(harness.files_left_in_downloads().is_empty());
    }

    #[gpui::test]
    async fn test_dropping_every_caller_cancels_the_install(cx: &mut TestAppContext) {
        struct SetOnDrop(Rc<Cell<bool>>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let cancelled = Rc::new(Cell::new(false));
        let (_finish_tx, finish_rx) = oneshot::channel::<()>();
        let first = cx.update(|cx| {
            join_or_start(None, cx, {
                let cancelled = cancelled.clone();
                move |cx| {
                    cx.spawn(async move |_| {
                        let _guard = SetOnDrop(cancelled);
                        finish_rx.await.expect("never finished by the test");
                        Err(anyhow!("unreachable"))
                    })
                }
            })
        });
        let second = cx.update(|cx| join_or_start(None, cx, |_| unreachable!("it is joined")));
        cx.run_until_parked();

        drop(first);
        cx.run_until_parked();
        assert!(!cancelled.get(), "one caller is still waiting");
        drop(second);
        cx.run_until_parked();
        assert!(cancelled.get(), "the last caller leaving cancels it");

        let starts = Rc::new(Cell::new(0));
        let third = cx.update(|cx| {
            join_or_start(None, cx, {
                let starts = starts.clone();
                move |_| {
                    starts.set(starts.get() + 1);
                    Task::ready(Err(anyhow!("a fresh attempt")))
                }
            })
        });
        assert!(third.await.is_err());
        assert_eq!(starts.get(), 1, "a cancelled install is not joined");
    }

    #[gpui::test]
    async fn test_the_pinned_version_is_a_floor(cx: &mut TestAppContext) {
        let rolled_back = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        rolled_back.write_state(&InstallerState {
            installed_version: Some("1.2.2".to_string()),
            ..InstallerState::default()
        });
        let mut ui = rolled_back.ui(&[true]);
        let binary = rolled_back
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("the pinned version");
        assert_eq!(binary.version, "1.2.3");
        assert_eq!(
            ui.prompts,
            vec![(
                ConsentRequest::Update {
                    from: version("1.2.2"),
                    to: version("1.2.3"),
                },
                Vec::new()
            )]
        );

        let mut raised = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        raised
            .run(Goal::Ensure, &mut raised.ui(&[true]))
            .await
            .expect("1.2.3");
        raised.installer.source = Arc::new(ReleaseSource {
            pinned_version: version("1.2.4"),
            ..test_key_source()
        });
        let mut ui = raised.ui(&[true]);
        let binary = raised
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("a raised pin reaches an existing install");
        assert_eq!(binary.version, "1.2.4");
        assert_eq!(
            ui.prompts,
            vec![(
                ConsentRequest::Update {
                    from: version("1.2.3"),
                    to: version("1.2.4"),
                },
                Vec::new()
            )]
        );
    }

    #[gpui::test]
    async fn test_a_record_made_under_another_key_is_not_reused(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let first = harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        let mut state = harness.state();
        for verified in state.verified_binaries.values_mut() {
            verified.signing_key = TEST_OTHER_KEY_FINGERPRINT.to_string();
        }
        harness.write_state(&state);
        let before = harness.server.requests().len();

        let mut ui = harness.ui(&[]);
        let second = harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("checked against the manifests again");
        assert_eq!(second, first);
        assert!(ui.prompts.is_empty());
        assert_eq!(
            harness.server.requests()[before..],
            release_requests("1.2.3", "linux-x64")[..4]
        );
        assert_eq!(
            harness.state().verified_binaries["1.2.3/linux-x64"].signing_key,
            TEST_RELEASE_KEY_FINGERPRINT
        );
    }

    #[gpui::test]
    async fn test_a_tampered_download_is_refused_and_nothing_is_left(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let download_url = release_url("1.2.3/linux-x64/claude.zst");
        let mut contents = harness.server.get(&download_url);
        let last = contents.len() - 1;
        contents[last] ^= 0x01;
        harness.server.set(download_url, contents);

        let error = harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect_err("tampered download");
        assert_eq!(error.to_string(), CHECKSUM_REFUSED);
        assert_eq!(harness.wsl.stage_count(), 0);
        assert!(harness.files_left_in_downloads().is_empty());
        assert!(harness.state().verified_binaries.is_empty());
    }

    #[gpui::test]
    async fn test_a_download_of_the_wrong_size_is_refused(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let download_url = release_url("1.2.3/linux-x64/claude.zst");
        let original = harness.server.get(&download_url);

        let mut truncated = original.clone();
        truncated.pop();
        harness.server.set(download_url.clone(), truncated);
        let error = harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect_err("truncated download");
        assert_eq!(error.to_string(), SIZE_REFUSED);

        let mut extended = original;
        extended.push(0);
        harness.server.set(download_url, extended);
        let error = harness
            .run(Goal::Ensure, &mut harness.ui(&[]))
            .await
            .expect_err("extended download");
        assert_eq!(error.to_string(), SIZE_REFUSED);

        assert_eq!(harness.wsl.stage_count(), 0);
        assert!(harness.files_left_in_downloads().is_empty());
    }

    #[gpui::test]
    async fn test_update_shows_and_installs_the_newer_version(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        let before = harness.server.requests().len();

        let mut ui = harness.ui(&[true]);
        let binary = harness
            .run(Goal::Update(ClaudeReleaseChannel::Stable), &mut ui)
            .await
            .expect("update");
        assert_eq!(binary.version, "1.2.4");
        assert_eq!(
            binary.sha256,
            expected_file("1.2.4/manifest.json", LinuxPlatform::X64).sha256
        );
        assert_eq!(
            ui.prompts,
            vec![(
                ConsentRequest::Update {
                    from: version("1.2.3"),
                    to: version("1.2.4"),
                },
                Vec::new()
            )]
        );
        let mut expected = vec![release_url("stable")];
        expected.extend(release_requests("1.2.4", "linux-x64"));
        assert_eq!(harness.server.requests()[before..], expected[..]);
        assert_eq!(harness.state().installed_version.as_deref(), Some("1.2.4"));

        let mut ui = harness.ui(&[]);
        let current = harness
            .run(Goal::Ensure, &mut ui)
            .await
            .expect("the updated version stays");
        assert_eq!(current, binary);
        assert!(ui.prompts.is_empty());
    }

    #[gpui::test]
    async fn test_update_refuses_an_older_version(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        harness
            .run(
                Goal::Update(ClaudeReleaseChannel::Latest),
                &mut harness.ui(&[true]),
            )
            .await
            .expect("update to 1.2.4");
        harness.server.set(release_url("stable"), b"1.2.3".to_vec());
        let before = harness.server.requests().len();

        let mut ui = harness.ui(&[]);
        let error = harness
            .run(Goal::Update(ClaudeReleaseChannel::Stable), &mut ui)
            .await
            .expect_err("downgrade");
        assert!(error.to_string().contains("より古い"), "{error:#}");
        assert!(ui.prompts.is_empty());
        assert_eq!(
            harness.server.requests()[before..],
            [release_url("stable")][..]
        );
        assert_eq!(harness.state().installed_version.as_deref(), Some("1.2.4"));
    }

    #[gpui::test]
    async fn test_update_refuses_a_manifest_for_another_version(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        for name in ["manifest.json", "manifest.json.sig"] {
            harness.server.set(
                release_url(&format!("1.2.4/{name}")),
                release_file(&format!("1.2.3/{name}")).to_vec(),
            );
        }

        let error = harness
            .run(
                Goal::Update(ClaudeReleaseChannel::Stable),
                &mut harness.ui(&[true]),
            )
            .await
            .expect_err("replayed manifest");
        assert!(error.to_string().contains("一致しません"), "{error:#}");
        assert!(
            !harness
                .server
                .requests()
                .contains(&release_url("1.2.4/linux-x64/claude.zst"))
        );
        assert_eq!(harness.state().installed_version.as_deref(), Some("1.2.3"));
    }

    #[gpui::test]
    async fn test_update_to_the_installed_version_changes_nothing(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[TEST_HOST]);
        let installed = harness
            .run(Goal::Ensure, &mut harness.ui(&[true]))
            .await
            .expect("first install");
        harness
            .server
            .set(release_url("stable"), b"1.2.3\n".to_vec());
        let before = harness.server.requests().len();

        let mut ui = harness.ui(&[]);
        let binary = harness
            .run(Goal::Update(ClaudeReleaseChannel::Stable), &mut ui)
            .await
            .expect("already current");
        assert_eq!(binary, installed);
        assert!(ui.prompts.is_empty());
        assert_eq!(
            harness.server.requests()[before..],
            [release_url("stable")][..]
        );
    }

    #[gpui::test]
    async fn test_declining_the_update_check_makes_no_request(cx: &mut TestAppContext) {
        let harness = Harness::new(cx, X64_PROBE, &[]);
        let mut ui = harness.ui(&[false]);
        let error = harness
            .run(Goal::Update(ClaudeReleaseChannel::Stable), &mut ui)
            .await
            .expect_err("declined");
        assert_eq!(error.to_string(), UPDATE_CHECK_DECLINED);
        assert_eq!(
            ui.prompts,
            vec![(
                ConsentRequest::CheckForUpdate {
                    channel: ClaudeReleaseChannel::Stable
                },
                vec![TEST_HOST.to_string()]
            )]
        );
        assert!(harness.server.requests().is_empty());
    }
}
