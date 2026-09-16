//! サンドボックス (WSL など) の中のプロセスが使うパスと、Zed (ホスト) のパスの相互変換
//! (ADR-F49 決定 4・23)。ファイルシステムに触れない部分だけで、IDE 連携 (`ide_server`) と
//! 外部エージェントの ACP (`agent_servers`) が共用する。
//!
//! サンドボックスから来るパスは信用しない入力として扱う。変換できないもの、`..` を含むもの、
//! ワークツリーの外を指すものはすべて拒否し、逆向き (ホスト → サンドボックス) でもワークツリーの
//! 外のパスは見せない。どちらも綴りの上での判定で、ワークツリーの中に置かれたリンクの行き先は
//! 見ない (それはファイルシステムに触れる `ide_server` の役目)。
//!
//! file URI の綴りは相手によって違う。Claude Code CLI の IDE 連携は `file://` に素のパスを付けて
//! 送り、受けた URI もデコードせずに自分のパスと比べる (CLI 2.1.270 の `normalizeFileUri`) ので、
//! [`PathMapper::uri_to_host`] と [`PathMapper::to_client_uri`] はエンコードもデコードもしない。
//! RFC 3986 どおりパーセントエンコードした file URI は [`SandboxPaths::client_file_uri_to_host_uri`]
//! と [`SandboxPaths::host_file_uri_to_client_uri`] で扱う。ACP の境界の URI は表示側の読み方に
//! 合わせる必要があるので、`agent_servers` がメンションとして分類してから変換する。

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

use crate::paths::PathStyle;

/// サンドボックス (クライアント) 側のパスとホストのパスの変換。クライアント側のパスは POSIX の
/// 絶対パス。
///
/// どちらの向きも、変換できない・`..` を含む・相手側で別の意味になるパスには `None`
/// を返す。ワークツリーの中かどうかはここでは見ない (それは [`SandboxPaths`] の役目)。
pub trait PathMapper: Send + Sync {
    fn to_host(&self, client_path: &str) -> Option<PathBuf>;
    fn to_client(&self, host_path: &Path) -> Option<String>;

    /// ホストのパスの綴り。実行している OS からは決めない (Linux の上でも WSL の変換を
    /// 確かめられるように)。
    fn host_path_style(&self) -> PathStyle;

    /// `file://` の URI も素のパスも受ける。CLI の `normalizeFileUri` に合わせて
    /// パーセントデコードはしない。
    fn uri_to_host(&self, client_uri: &str) -> Option<PathBuf> {
        self.to_host(client_uri.strip_prefix("file://").unwrap_or(client_uri))
    }

    /// CLI は受けた URI をデコードせずに自分のパスと比べるので、エンコードしない。
    fn to_client_uri(&self, host_path: &Path) -> Option<String> {
        self.to_client(host_path)
            .map(|client_path| format!("file://{client_path}"))
    }
}

/// Windows のホストと、WSL のディストリビューションの中のプロセス。
///
/// - `/mnt/<drive>/…` ↔ `<DRIVE>:\…` (扱う自動マウント先は既定の `/mnt` だけ)
/// - それ以外のディストリビューション内のパス ↔ `\\wsl.localhost\<distro>\…`
///   (ホスト側の入力では `\\wsl$\<distro>\…` も受ける)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslPathMapper {
    pub distro: String,
}

impl WslPathMapper {
    /// ワークツリーのルートが `roots` のプロジェクトを、`sandbox_distro` のサンドボックスから
    /// 見るための変換器。`None` は既定のディストリビューションで、Zed はその名前を知らない。
    ///
    /// ディストリビューションの中のフォルダーは、そのディストリビューションのサンドボックスから
    /// しか同じ場所に見えない。複数のディストリビューションにまたがるプロジェクトや、WSL から
    /// 届かないフォルダー (ネットワーク共有) を含むプロジェクトは、どのサンドボックスからも全体を
    /// 見られないので断る。
    pub fn for_worktree_roots(
        roots: &[PathBuf],
        sandbox_distro: Option<&str>,
    ) -> Result<Self, WslWorktreeRootsError> {
        if let Some(sandbox_distro) = sandbox_distro
            && !is_valid_distro_name(sandbox_distro)
        {
            return Err(WslWorktreeRootsError::InvalidDistroName(
                sandbox_distro.to_string(),
            ));
        }
        let mut project_distro: Option<&str> = None;
        for root in roots {
            let parsed = root
                .to_str()
                .and_then(WindowsPath::parse)
                .ok_or_else(|| WslWorktreeRootsError::NotAWindowsPath(root.clone()))?;
            match parsed.root {
                WindowsRoot::Drive(_) => {}
                WindowsRoot::Unc { server, share } if is_wsl_server(server) => {
                    // サンドボックスからは `/mnt/c/…` に見え、戻すと `C:\…` という別の綴りになる。
                    if is_drive_mount(&parsed.components) {
                        return Err(WslWorktreeRootsError::DriveSeenThroughWsl(root.clone()));
                    }
                    if let Some(distro) = project_distro
                        && !distro.eq_ignore_ascii_case(share)
                    {
                        return Err(WslWorktreeRootsError::MultipleDistros {
                            first: distro.to_string(),
                            second: share.to_string(),
                        });
                    }
                    project_distro = Some(share);
                }
                WindowsRoot::Unc { .. } => {
                    return Err(WslWorktreeRootsError::NetworkShare(root.clone()));
                }
            }
        }
        let distro = match (project_distro, sandbox_distro) {
            (Some(project), Some(sandbox)) if !project.eq_ignore_ascii_case(sandbox) => {
                return Err(WslWorktreeRootsError::DistroMismatch {
                    project: project.to_string(),
                    sandbox: sandbox.to_string(),
                });
            }
            (Some(project), None) => {
                return Err(WslWorktreeRootsError::DefaultDistroUnknown {
                    project: project.to_string(),
                });
            }
            (_, Some(sandbox)) => sandbox.to_string(),
            (None, None) => String::new(),
        };
        Ok(Self { distro })
    }
}

/// Why [`WslPathMapper::for_worktree_roots`] refused. `Display` is the English
/// wording for logs (ADR-F07); a UI-facing sentence is built from the variant
/// by the caller, since util has no UI strings of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WslWorktreeRootsError {
    /// The name given as the sandbox's own distribution is not a valid one.
    InvalidDistroName(String),
    /// A worktree root is not an absolute Windows path at all.
    NotAWindowsPath(PathBuf),
    /// A worktree root is a Windows drive seen through a WSL distribution's
    /// `\\wsl.localhost\<distro>\mnt\<drive>\…` share, which round-trips to a
    /// different spelling (`<drive>:\…`) than it started from.
    DriveSeenThroughWsl(PathBuf),
    /// A worktree root is on a network share, which a WSL sandbox cannot
    /// reach at all.
    NetworkShare(PathBuf),
    /// The worktree has roots in two different WSL distributions; a sandbox
    /// runs in only one.
    MultipleDistros { first: String, second: String },
    /// The worktree's distribution and the sandbox's distribution differ.
    DistroMismatch { project: String, sandbox: String },
    /// The worktree is inside a WSL distribution, but the sandbox runs in the
    /// default distribution, whose name is not known.
    DefaultDistroUnknown { project: String },
}

impl fmt::Display for WslWorktreeRootsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDistroName(name) => {
                write!(formatter, "{name:?} is not a WSL distribution name")
            }
            Self::NotAWindowsPath(path) => write!(
                formatter,
                "{} is not an absolute Windows path",
                path.display()
            ),
            Self::DriveSeenThroughWsl(path) => write!(
                formatter,
                "{} is a Windows drive seen through WSL; open it by its drive letter",
                path.display()
            ),
            Self::NetworkShare(path) => write!(
                formatter,
                "{} is on a network share, which a WSL sandbox cannot reach",
                path.display()
            ),
            Self::MultipleDistros { first, second } => write!(
                formatter,
                "the project has folders in two WSL distributions ({first} and {second}), and a \
                 sandbox runs in only one"
            ),
            Self::DistroMismatch { project, sandbox } => write!(
                formatter,
                "the project is in the WSL distribution {project}, but the sandbox runs in \
                 {sandbox}"
            ),
            Self::DefaultDistroUnknown { project } => write!(
                formatter,
                "the project is in the WSL distribution {project}, and the name of the default \
                 distribution the sandbox runs in is not known"
            ),
        }
    }
}

impl std::error::Error for WslWorktreeRootsError {}

impl PathMapper for WslPathMapper {
    fn to_host(&self, client_path: &str) -> Option<PathBuf> {
        let components = client_components(client_path, NameRules::Windows)?;
        if let [mount, drive, rest @ ..] = components.as_slice()
            && *mount == "mnt"
            && let Some(letter) = drive_letter(drive)
        {
            return Some(PathBuf::from(format!(
                "{}:\\{}",
                letter.to_ascii_uppercase(),
                rest.join("\\")
            )));
        }
        if !is_valid_distro_name(&self.distro) {
            return None;
        }
        Some(PathBuf::from(format!(
            "\\\\wsl.localhost\\{}\\{}",
            self.distro,
            components.join("\\")
        )))
    }

    fn to_client(&self, host_path: &Path) -> Option<String> {
        let parsed = WindowsPath::parse(host_path.to_str()?)?;
        let mut client_path = match parsed.root {
            WindowsRoot::Drive(letter) => format!("/mnt/{}", letter.to_ascii_lowercase()),
            WindowsRoot::Unc { server, share } => {
                // ディストリビューションの共有から見たドライブは、`/mnt/c/…` を経て `C:\…` という
                // 別の綴りで戻ってくる。
                if !is_wsl_server(server)
                    || !share.eq_ignore_ascii_case(&self.distro)
                    || is_drive_mount(&parsed.components)
                {
                    return None;
                }
                String::new()
            }
        };
        for component in &parsed.components {
            // サンドボックスから戻ってくるパスと同じ規則で断る (`a.rs::$DATA` のような代替データ
            // ストリームやデバイス名は、戻せないうえに別のものを開く)。
            if !is_portable_windows_name(component) {
                return None;
            }
            client_path.push('/');
            client_path.push_str(component);
        }
        if client_path.is_empty() {
            client_path.push('/');
        }
        Some(client_path)
    }

    fn host_path_style(&self) -> PathStyle {
        PathStyle::Windows
    }
}

/// ホストとサンドボックスが同じパスを使う環境 (Windows 以外のサンドボックス)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdentityPathMapper;

impl PathMapper for IdentityPathMapper {
    fn to_host(&self, client_path: &str) -> Option<PathBuf> {
        let components = client_components(client_path, NameRules::Posix)?;
        Some(PathBuf::from(format!("/{}", components.join("/"))))
    }

    fn to_client(&self, host_path: &Path) -> Option<String> {
        let components = client_components(host_path.to_str()?, NameRules::Posix)?;
        Some(format!("/{}", components.join("/")))
    }

    fn host_path_style(&self) -> PathStyle {
        PathStyle::Unix
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NameRules {
    /// 名前はホストの Windows で解釈される。
    Windows,
    Posix,
}

/// サンドボックスの絶対パスを成分に分ける。`.` と空の成分 (`//`) は落とし、上の階層を指す
/// 成分を含むものは `None`。
fn client_components(client_path: &str, rules: NameRules) -> Option<Vec<&str>> {
    let relative = client_path.strip_prefix('/')?;
    let mut components = Vec::new();
    for component in relative.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." || component.contains('\0') {
            return None;
        }
        if rules == NameRules::Windows && !is_portable_windows_name(component) {
            return None;
        }
        components.push(component);
    }
    Some(components)
}

/// Win32 のパス正規化は名前の末尾の `.` と空白を落とすので、そういう名前は別の名前
/// (点と空白だけなら `.` や `..`) と同じ場所を指しうる。`\` は区切り、`:` は代替データ
/// ストリーム、デバイス名はファイルではないものを開く。
fn is_portable_windows_name(component: &str) -> bool {
    if component.ends_with(['.', ' ']) {
        return false;
    }
    let has_reserved_character = component.chars().any(|character| {
        character < ' ' || matches!(character, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    });
    !has_reserved_character && !is_reserved_device_name(component)
}

fn is_reserved_device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    // Windows は上付きの ¹²³ も COM・LPT の番号として扱う。
    let Some(number) = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
    else {
        return false;
    };
    let mut characters = number.chars();
    matches!(
        (characters.next(), characters.next()),
        (Some('0'..='9' | '¹' | '²' | '³'), None)
    )
}

fn drive_letter(component: &str) -> Option<char> {
    let mut characters = component.chars();
    let letter = characters.next()?;
    (letter.is_ascii_alphabetic() && characters.next().is_none()).then_some(letter)
}

/// ディストリビューションの中の `/mnt/<drive>` (Windows のドライブの自動マウント先) の下か。
fn is_drive_mount(components: &[&str]) -> bool {
    matches!(components, [mount, drive, ..] if *mount == "mnt" && drive_letter(drive).is_some())
}

fn is_valid_distro_name(distro: &str) -> bool {
    !distro.is_empty()
        && !distro.chars().all(|character| character == '.')
        && distro.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn is_wsl_server(server: &str) -> bool {
    server.eq_ignore_ascii_case("wsl.localhost") || server.eq_ignore_ascii_case("wsl$")
}

/// Windows のパスが、ディストリビューション自身のファイルシステムを見せる WSL の共有
/// (`\\wsl.localhost\<distro>\…`、古い `\\wsl$\…`、`canonicalize` が返す
/// `\\?\UNC\wsl.localhost\…`) の中を指すか。
///
/// 綴りだけで決める (ファイルシステムにも `wsl.exe` にも触らない)。ファイル監視の登録の
/// たびに呼ばれるため。ドライブを WSL 越しに見た `\\wsl.localhost\<distro>\mnt\c\…` も
/// 9p の上にあることに変わりはないので `true`。
pub fn is_wsl_share_path(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    let Some(rest) = text
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| text.strip_prefix(r"\\"))
        .or_else(|| text.strip_prefix("//"))
    else {
        return false;
    };
    matches!(
        unc_root(rest),
        Some((WindowsRoot::Unc { server, .. }, _)) if is_wsl_server(server)
    )
}

/// ADR-F49 決定 34: ディストリビューションの中のフォルダーを指す書込許可を、保存と
/// 比較に使う唯一の綴り (`\\wsl.localhost\<distro 小文字>\…`) に畳む。WSL の共有でない
/// パス (ドライブ、他のサーバーの共有、素の Linux のパス) と、ディストリビューションの
/// ルート自身は `None` = 畳まずそのまま扱う。
///
/// 畳むのはサーバー名 (`\\wsl$\` は同じ場所の古い綴り・`\\?\UNC\` は `canonicalize` が
/// 返す形) と ASCII の大文字小文字だけで、その先の名前は ext4 では区別されるので触らない。
/// `sandbox` が承認時に作る綴り (`wsl_share_path_spelling`) と同じ規則。決定 34 より前に
/// 保存された `\\wsl.localhost\Ubuntu\…` の行が、同じ場所の新しい許可と一致せず再承認に
/// なっていたのを直すために、設定の読み込み時と設定ファイルの移行で使う。
///
/// `..` を含むパスは `None`: 畳んだ先が利用者に見せた場所とは限らない。
pub fn canonical_wsl_grant_spelling(path: &str) -> Option<String> {
    let parsed = WindowsPath::parse(path)?;
    let WindowsRoot::Unc { server, share } = parsed.root else {
        return None;
    };
    if !is_wsl_server(server) || parsed.components.is_empty() {
        return None;
    }
    let mut spelling = format!(r"\\wsl.localhost\{}", share.to_ascii_lowercase());
    for component in parsed.components {
        spelling.push('\\');
        spelling.push_str(component);
    }
    Some(spelling)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowsRoot<'a> {
    Drive(char),
    Unc { server: &'a str, share: &'a str },
}

/// ホストの Windows パスを文字列のまま読んだもの。`Path::components` は実行している
/// OS の規則でしか読めないので、ここでは自前で分ける。
struct WindowsPath<'a> {
    root: WindowsRoot<'a>,
    components: Vec<&'a str>,
}

impl<'a> WindowsPath<'a> {
    /// `\` と `/` の両方を区切りとして読む。`canonicalize` が返す `\\?\` 付きの形も受ける。
    fn parse(text: &'a str) -> Option<Self> {
        let (root, rest) = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            unc_root(rest)?
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            drive_root(rest)?
        } else if let Some(rest) = text.strip_prefix(r"\\").or_else(|| text.strip_prefix("//")) {
            unc_root(rest)?
        } else {
            drive_root(text)?
        };
        let mut components = Vec::new();
        for component in rest.split(['\\', '/']) {
            match component {
                "" | "." => {}
                ".." => return None,
                component => components.push(component),
            }
        }
        Some(Self { root, components })
    }
}

fn drive_root(text: &str) -> Option<(WindowsRoot<'_>, &str)> {
    let mut characters = text.chars();
    let letter = characters.next().filter(char::is_ascii_alphabetic)?;
    // `C:` だけや `C:foo` は、ドライブごとのカレントディレクトリからの相対パス。
    let rest = characters
        .as_str()
        .strip_prefix(':')?
        .strip_prefix(['\\', '/'])?;
    Some((WindowsRoot::Drive(letter.to_ascii_uppercase()), rest))
}

fn unc_root(text: &str) -> Option<(WindowsRoot<'_>, &str)> {
    let mut parts = text.splitn(3, ['\\', '/']);
    // `.` と `?` はデバイスの名前空間 (`\\.\`・`//?/`) で、ネットワークの共有ではない。
    let is_name = |part: &&str| !matches!(*part, "" | "." | ".." | "?");
    let server = parts.next().filter(is_name)?;
    let share = parts.next().filter(is_name)?;
    Some((
        WindowsRoot::Unc { server, share },
        parts.next().unwrap_or_default(),
    ))
}

/// ワークツリーの中かどうかを成分単位で比べるための形。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComparablePath {
    root: String,
    components: Vec<String>,
}

impl ComparablePath {
    /// Windows のパスでは、ドライブ文字と UNC のサーバー名・共有名だけは大文字小文字を区別しない
    /// (CLI はドライブを小文字で返し、`\\wsl$\` と `\\wsl.localhost\` は同じ場所)。その先の名前は
    /// ワークツリーのルートから作ったパスが同じ綴りで戻ってくるので、厳密に比べる。8.3 の短い名前の
    /// ような別の綴りは、別の場所 (ワークツリーの外) とみなす。
    pub fn new(path: &Path, style: PathStyle) -> Option<Self> {
        let text = path.to_str()?;
        match style {
            PathStyle::Windows => {
                let parsed = WindowsPath::parse(text)?;
                let root = match parsed.root {
                    WindowsRoot::Drive(letter) => format!("{letter}:"),
                    WindowsRoot::Unc { server, share } => {
                        let server = if is_wsl_server(server) {
                            "wsl.localhost".to_string()
                        } else {
                            server.to_lowercase()
                        };
                        format!(r"\\{server}\{}", share.to_lowercase())
                    }
                };
                Some(Self {
                    root,
                    components: parsed.components.into_iter().map(str::to_string).collect(),
                })
            }
            PathStyle::Unix => Some(Self {
                root: "/".to_string(),
                components: client_components(text, NameRules::Posix)?
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            }),
        }
    }

    /// ファイルシステムから得たパスを、実行している OS の綴りで読む。
    pub fn local(path: &Path) -> Option<Self> {
        Self::new(path, PathStyle::local())
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn components(&self) -> &[String] {
        &self.components
    }

    pub fn is_unc(&self) -> bool {
        self.root.starts_with(r"\\")
    }

    pub fn starts_with(&self, root: &Self) -> bool {
        self.root == root.root && self.components.starts_with(&root.components)
    }
}

/// 実行している OS の 2 つのパスが (綴りの揺れを除いて) 同じ場所を指すか。
pub fn same_location(left: &Path, right: &Path) -> bool {
    match (ComparablePath::local(left), ComparablePath::local(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// サンドボックスのパスを通さなかった理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPathError {
    /// 相手側に同じ場所を指す綴りが無い (`..` を含む、相対パス、読めない URI、別の
    /// ディストリビューションなど)。
    Unmappable,
    OutsideWorkspace,
}

impl fmt::Display for SandboxPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unmappable => "the path has no counterpart between the sandbox and the host",
            Self::OutsideWorkspace => "the path is outside the workspace",
        })
    }
}

impl std::error::Error for SandboxPathError {}

/// サンドボックスのセッション 1 本ぶんのパスの規則: 変換器と、サンドボックスに見せてよい
/// ワークツリーのルート (ホストのパス)。
pub struct SandboxPaths {
    mapper: Arc<dyn PathMapper>,
    roots: Vec<PathBuf>,
}

impl SandboxPaths {
    pub fn new(mapper: Arc<dyn PathMapper>, roots: Vec<PathBuf>) -> Self {
        Self { mapper, roots }
    }

    pub fn mapper(&self) -> &dyn PathMapper {
        self.mapper.as_ref()
    }

    pub fn host_path_style(&self) -> PathStyle {
        self.mapper.host_path_style()
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// CLI から来たパス (か `file://` URI) を、ワークツリーの中のホストのパスに直す。
    /// 返すのはワークツリーのルートの綴りに合わせた形 ([`Self::in_root_spelling`])。
    pub fn to_host(&self, client_path: &str) -> Result<PathBuf> {
        let host_path = self
            .mapper
            .uri_to_host(client_path)
            .with_context(|| format!("{client_path} is not a path Zed can open"))?;
        self.in_root_spelling(&host_path)
            .with_context(|| format!("{client_path} is outside the workspace"))
    }

    /// サンドボックスの素のパス (URI は受けない) を、ワークツリーの中のホストのパスに直す。
    /// 返すのはワークツリーのルートの綴りに合わせた形 ([`Self::in_root_spelling`])。
    pub fn client_path_to_host(&self, client_path: &str) -> Result<PathBuf, SandboxPathError> {
        let host_path = self
            .mapper
            .to_host(client_path)
            .ok_or(SandboxPathError::Unmappable)?;
        self.in_root_spelling(&host_path)
            .ok_or(SandboxPathError::OutsideWorkspace)
    }

    pub fn to_client(&self, host_path: &Path) -> Option<String> {
        if !self.contains(host_path) {
            return None;
        }
        self.mapper.to_client(host_path)
    }

    pub fn to_client_uri(&self, host_path: &Path) -> Option<String> {
        if !self.contains(host_path) {
            return None;
        }
        self.mapper.to_client_uri(host_path)
    }

    /// サンドボックスの RFC 3986 の file URI を、ワークツリーの中のホストの file URI に直す。
    /// `?` と `#` から後ろ (Zed のメンションの記号名や行の範囲) と、ディレクトリを表す末尾の
    /// `/` はそのまま運ぶ。
    pub fn client_file_uri_to_host_uri(
        &self,
        client_uri: &str,
    ) -> Result<String, SandboxPathError> {
        let parts = FileUriParts::parse(client_uri).ok_or(SandboxPathError::Unmappable)?;
        if !parts.has_local_authority() {
            return Err(SandboxPathError::Unmappable);
        }
        let host_path = self.client_path_to_host(&parts.path)?;
        let host_uri = host_file_uri(&host_path, self.host_path_style())
            .ok_or(SandboxPathError::Unmappable)?;
        Ok(parts.finish(host_uri, PathStyle::Unix))
    }

    /// ホストの file URI (Windows では `file:///C:/…` と UNC の `file://server/share/…`・
    /// `file:////server/share/…`) を、サンドボックスの RFC 3986 の file URI に直す。
    pub fn host_file_uri_to_client_uri(&self, host_uri: &str) -> Result<String, SandboxPathError> {
        let parts = FileUriParts::parse(host_uri).ok_or(SandboxPathError::Unmappable)?;
        let host_path = parts
            .host_path(self.host_path_style())
            .ok_or(SandboxPathError::Unmappable)?;
        if !self.contains(&host_path) {
            return Err(SandboxPathError::OutsideWorkspace);
        }
        let client_path = self
            .mapper
            .to_client(&host_path)
            .ok_or(SandboxPathError::Unmappable)?;
        Ok(parts.finish(
            format!("file://{}", encode_uri_path(&client_path)),
            self.host_path_style(),
        ))
    }

    pub fn contains(&self, host_path: &Path) -> bool {
        let Some(candidate) = self.comparable(host_path) else {
            return false;
        };
        self.roots
            .iter()
            .filter_map(|root| self.comparable(root))
            .any(|root| candidate.starts_with(&root))
    }

    /// 変換器のホストの綴りで読んだ、比べるための形。
    pub fn comparable(&self, host_path: &Path) -> Option<ComparablePath> {
        ComparablePath::new(host_path, self.host_path_style())
    }

    /// ワークツリーのルートの綴りに合わせて組み直したホストのパス。ルートの外なら `None`。
    ///
    /// Zed のワークツリーの `abs_path` はプロジェクトを開いたときの綴りのままで、WSL の共有を
    /// 開くと Windows の `canonicalize` が返す `\\?\UNC\wsl.localhost\<distro>\…` になる。
    /// `Project` がパスからワークツリーを見つけるのは綴りの前方一致 (`PathStyle::strip_prefix`)
    /// なので、変換器が作る `\\wsl.localhost\<distro>\…` のままでは、同じ場所を指していても
    /// プロジェクトのどのフォルダーにも入っていないと判定され、`fs/read_text_file` も
    /// `fs/write_text_file` も通らない (ADR-F49 決定 23)。どの綴りで書かれたルートでも
    /// Zed がそのまま使えるように、ここでルートの綴りに戻す。
    fn in_root_spelling(&self, host_path: &Path) -> Option<PathBuf> {
        let style = self.host_path_style();
        let (root, below_root) = self.split_at_root(host_path)?;
        let mut spelling = root.to_str()?.to_string();
        for name in below_root {
            if !spelling.ends_with(style.separators_ch()) {
                spelling.push_str(style.primary_separator());
            }
            spelling.push_str(&name);
        }
        Some(PathBuf::from(spelling))
    }

    /// `host_path` を含むワークツリーのルートと、そのルートより下の名前。
    pub fn split_at_root(&self, host_path: &Path) -> Option<(&Path, Vec<String>)> {
        let candidate = self.comparable(host_path)?;
        self.roots.iter().find_map(|root| {
            let comparable = self.comparable(root)?;
            candidate.starts_with(&comparable).then(|| {
                let below_root = candidate
                    .components
                    .iter()
                    .skip(comparable.components.len())
                    .cloned()
                    .collect();
                (root.as_path(), below_root)
            })
        })
    }
}

/// file URI のパスにそのまま書けない文字 (RFC 3986 の pchar でないもの)。`/` は区切りなので
/// 残す。`%` は名前の一部として必ずエンコードする (`url` の `set_path` は `%` をエンコード済みと
/// みなして残すので、`a%20b` という名前が相手には `a b` に見える)。
const FILE_URI_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

fn encode_uri_path(path: &str) -> String {
    utf8_percent_encode(path, FILE_URI_PATH).to_string()
}

fn strip_file_scheme(uri: &str) -> Option<&str> {
    let (scheme, rest) = uri.split_at_checked("file:".len())?;
    scheme.eq_ignore_ascii_case("file:").then_some(rest)
}

/// RFC 3986 の file URI を分けたもの。
struct FileUriParts<'a> {
    authority: &'a str,
    /// パーセントデコード済み。
    path: String,
    /// `?` か `#` から後ろ。
    suffix: &'a str,
}

impl<'a> FileUriParts<'a> {
    /// `..` はここでは解決しない (`url` の解析は `..` と `%2E%2E` を先に畳んでしまう)。デコード
    /// したパスを、それぞれの側の規則で拒否する。前後の空白やバッククォートは受けない。
    fn parse(uri: &'a str) -> Option<Self> {
        let rest = strip_file_scheme(uri)?.strip_prefix("//")?;
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, rest) = rest.split_at(authority_end);
        if !rest.starts_with('/') {
            return None;
        }
        let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
        let (encoded_path, suffix) = rest.split_at(path_end);
        let path = percent_decode_str(encoded_path).decode_utf8().ok()?;
        // デコードで現れた NUL は、受け取った側でパスの終わりとして読まれうる。
        if path.contains('\0') {
            return None;
        }
        Some(Self {
            authority,
            path: path.into_owned(),
            suffix,
        })
    }

    fn has_local_authority(&self) -> bool {
        self.authority.is_empty() || self.authority.eq_ignore_ascii_case("localhost")
    }

    fn host_path(&self, style: PathStyle) -> Option<PathBuf> {
        match style {
            PathStyle::Windows => {
                let text = if !self.has_local_authority() {
                    format!(r"\\{}{}", self.authority, self.path)
                } else if let Some(unc) = self.path.strip_prefix("//") {
                    format!(r"\\{unc}")
                } else {
                    let drive_path = self.path.strip_prefix('/')?;
                    // `file:///C:` はドライブのルート。`C:` だけではカレントディレクトリからの
                    // 相対パスになる。
                    if drive_path.len() == 2 {
                        format!(r"{drive_path}\")
                    } else {
                        drive_path.to_string()
                    }
                };
                WindowsPath::parse(&text)?;
                Some(PathBuf::from(text))
            }
            PathStyle::Unix => self
                .has_local_authority()
                .then(|| PathBuf::from(&self.path)),
        }
    }

    /// `style` はこの URI を書いた側の綴り。POSIX の名前は `\` を含みうるので、そちらでは
    /// `\` をディレクトリの印とみなさない。
    fn finish(&self, mut uri: String, style: PathStyle) -> String {
        let names_directory = match style {
            PathStyle::Windows => self.path.ends_with(['/', '\\']),
            PathStyle::Unix => self.path.ends_with('/'),
        };
        if names_directory && !uri.ends_with('/') {
            uri.push('/');
        }
        uri.push_str(self.suffix);
        uri
    }
}

fn host_file_uri(host_path: &Path, style: PathStyle) -> Option<String> {
    let text = host_path.to_str()?;
    let (mut uri, components, ends_at_root) = match style {
        PathStyle::Windows => {
            let parsed = WindowsPath::parse(text)?;
            match parsed.root {
                WindowsRoot::Drive(letter) => {
                    (format!("file:///{letter}:"), parsed.components, true)
                }
                WindowsRoot::Unc { server, share } => (
                    format!("file://{server}/{}", encode_uri_path(share)),
                    parsed.components,
                    false,
                ),
            }
        }
        PathStyle::Unix => (
            "file://".to_string(),
            client_components(text, NameRules::Posix)?,
            true,
        ),
    };
    for component in &components {
        uri.push('/');
        uri.push_str(&encode_uri_path(component));
    }
    // ドライブや `/` そのものは `/` で終えないとルートとして読まれない。
    if components.is_empty() && ends_at_root {
        uri.push('/');
    }
    Some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl() -> WslPathMapper {
        WslPathMapper {
            distro: "Ubuntu".to_string(),
        }
    }

    fn host(client_path: &str) -> Option<String> {
        wsl()
            .to_host(client_path)
            .map(|path| path.to_string_lossy().into_owned())
    }

    fn client(host_path: &str) -> Option<String> {
        wsl().to_client(Path::new(host_path))
    }

    #[test]
    fn maps_drive_paths_to_the_host() {
        assert_eq!(
            host("/mnt/i/zed-jp-llm/src/main.rs").as_deref(),
            Some(r"I:\zed-jp-llm\src\main.rs")
        );
        assert_eq!(host("/mnt/c").as_deref(), Some(r"C:\"));
        assert_eq!(host("/mnt/c/").as_deref(), Some(r"C:\"));
        assert_eq!(host("/mnt/i/./a//b").as_deref(), Some(r"I:\a\b"));
        assert_eq!(
            host("/mnt/i/My Project/a b.rs").as_deref(),
            Some(r"I:\My Project\a b.rs")
        );
        assert_eq!(
            host("/mnt/i/プロジェクト/日本語.rs").as_deref(),
            Some(r"I:\プロジェクト\日本語.rs")
        );
        // 先頭の点、途中の点、デバイス名に似ているだけの名前は通す。
        assert_eq!(
            host("/mnt/i/.hidden/a.b/COM10.txt").as_deref(),
            Some(r"I:\.hidden\a.b\COM10.txt")
        );
    }

    #[test]
    fn maps_distro_paths_to_the_wsl_share() {
        assert_eq!(
            host("/home/user/project/a.rs").as_deref(),
            Some(r"\\wsl.localhost\Ubuntu\home\user\project\a.rs")
        );
        assert_eq!(host("/").as_deref(), Some(r"\\wsl.localhost\Ubuntu\"));
        // `/mnt` の下でも 1 文字の英字でなければドライブではない。
        assert_eq!(
            host("/mnt/data/a.rs").as_deref(),
            Some(r"\\wsl.localhost\Ubuntu\mnt\data\a.rs")
        );
        assert_eq!(
            host("/mnt/1/a.rs").as_deref(),
            Some(r"\\wsl.localhost\Ubuntu\mnt\1\a.rs")
        );
    }

    #[test]
    fn rejects_client_paths_that_do_not_map() {
        for client_path in [
            "",
            "relative/a.rs",
            "mnt/i/a.rs",
            r"C:\a.rs",
            "/mnt/i/a/../b",
            "/mnt/i/..",
            "/mnt/i/a/..",
            "/mnt/i/.. ",
            "/mnt/i/...",
            "/mnt/i/. .",
            r"/mnt/i/a\..\..\b",
            "/mnt/i/a.rs:secret",
            "/mnt/i/a.rs::$DATA",
            "/mnt/i/a?.rs",
            "/mnt/i/nul",
            "/mnt/i/COM1.txt",
            "/mnt/i/con .rs",
            "/mnt/i/trailing.",
            "/mnt/i/trailing ",
            "/mnt/i/dir./a.rs",
            "/mnt/i/COM¹",
            "/mnt/i/lpt³.txt",
            "/mnt/i/Com2.log",
            "/mnt/i/a\0b",
            "/home/u/../../etc",
        ] {
            assert_eq!(
                wsl().to_host(client_path),
                None,
                "{client_path:?} must not map"
            );
        }
        for distro in ["", "..", r"a\b", "a/b"] {
            let mapper = WslPathMapper {
                distro: distro.to_string(),
            };
            assert_eq!(mapper.to_host("/home/u"), None, "distro {distro:?}");
            // ドライブの変換はディストリビューション名に依らない。
            assert_eq!(mapper.to_host("/mnt/c/x"), Some(PathBuf::from(r"C:\x")));
        }
    }

    #[test]
    fn maps_host_paths_to_the_client() {
        assert_eq!(
            client(r"I:\zed-jp-llm\src\main.rs").as_deref(),
            Some("/mnt/i/zed-jp-llm/src/main.rs")
        );
        assert_eq!(client(r"i:\x").as_deref(), Some("/mnt/i/x"));
        assert_eq!(client("I:/x/y").as_deref(), Some("/mnt/i/x/y"));
        assert_eq!(client(r"\\?\I:\x").as_deref(), Some("/mnt/i/x"));
        assert_eq!(client(r"C:\").as_deref(), Some("/mnt/c"));
        assert_eq!(
            client(r"\\wsl.localhost\Ubuntu\home\u\x").as_deref(),
            Some("/home/u/x")
        );
        assert_eq!(client(r"\\wsl$\Ubuntu\home\u").as_deref(), Some("/home/u"));
        assert_eq!(
            client(r"\\WSL.LOCALHOST\ubuntu\home").as_deref(),
            Some("/home")
        );
        assert_eq!(
            client(r"\\?\UNC\wsl.localhost\Ubuntu\home").as_deref(),
            Some("/home")
        );
        assert_eq!(client(r"\\wsl.localhost\Ubuntu").as_deref(), Some("/"));
        assert_eq!(
            client(r"D:\データ\with space\ファイル.txt").as_deref(),
            Some("/mnt/d/データ/with space/ファイル.txt")
        );
        // `/mnt` の下でもドライブでない名前は、ディストリビューションの中のフォルダー。
        assert_eq!(
            client(r"\\wsl.localhost\Ubuntu\mnt\data\x").as_deref(),
            Some("/mnt/data/x")
        );
    }

    #[test]
    fn rejects_host_paths_that_do_not_map() {
        for host_path in [
            r"\\wsl.localhost\Debian\home",
            r"\\server\share\x",
            r"\\wsl.localhost",
            r"\\wsl.localhost\..\x",
            r"I:\a\..\b",
            r"relative\x",
            "C:",
            "C:relative",
            r"\\?\Volume{0}\x",
            "/home/u",
            // デバイスの名前空間。
            r"\\.\C:\proj\a.rs",
            r"\\.\pipe\x",
            "//?/C:/proj/a.rs",
            "//./C:/proj/a.rs",
            r"\??\C:\proj\a.rs",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\proj\a.rs",
            r"\\?\UNC\?\C:\proj",
            // 代替データストリーム・デバイス名・Win32 が落とす末尾の点。
            r"I:\proj\a.rs::$DATA",
            r"I:\proj\a.rs:secret",
            r"I:\proj\CON",
            r"I:\proj\trailing.",
            // ディストリビューションの共有から見たドライブは、戻すと別の綴りになる。
            r"\\wsl.localhost\Ubuntu\mnt\c\proj\a.rs",
            r"\\wsl$\Ubuntu\mnt\i",
        ] {
            assert_eq!(client(host_path), None, "{host_path:?} must not map");
        }
    }

    #[test]
    fn round_trips_between_the_client_and_the_host() {
        for client_path in [
            "/mnt/i/zed-jp-llm/src/main.rs",
            "/mnt/c",
            "/mnt/d/My Project/a b.rs",
            "/mnt/i/プロジェクト/日本語.rs",
            "/home/user/.config/x",
            "/",
        ] {
            let host_path = wsl()
                .to_host(client_path)
                .unwrap_or_else(|| panic!("{client_path:?} maps to the host"));
            assert_eq!(
                wsl().to_client(&host_path).as_deref(),
                Some(client_path),
                "{client_path:?} via {host_path:?}"
            );
        }
        for host_path in [
            r"I:\zed-jp-llm\src\main.rs",
            r"\\wsl.localhost\Ubuntu\home\u\データ.txt",
            r"D:\with space\x",
        ] {
            let client_path =
                client(host_path).unwrap_or_else(|| panic!("{host_path:?} maps to the client"));
            assert_eq!(
                wsl().to_host(&client_path),
                Some(PathBuf::from(host_path)),
                "{host_path:?} via {client_path:?}"
            );
        }
    }

    /// CLI 2.1.270 は `file://` に素のパスを付けて送り、受けた URI をデコードせずに比べるので、
    /// CLI 向けの URI はエンコードもデコードもしない。
    #[test]
    fn maps_cli_file_uris_without_percent_encoding() {
        assert_eq!(
            wsl().uri_to_host("file:///mnt/i/x/main.rs"),
            Some(PathBuf::from(r"I:\x\main.rs"))
        );
        assert_eq!(
            wsl().to_client_uri(Path::new(r"I:\x\main.rs")).as_deref(),
            Some("file:///mnt/i/x/main.rs")
        );
        assert_eq!(
            wsl().uri_to_host("file:///home/u/a.rs"),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu\home\u\a.rs"))
        );
        assert_eq!(
            wsl()
                .to_client_uri(Path::new(r"\\wsl$\Ubuntu\home\u\a b.rs"))
                .as_deref(),
            Some("file:///home/u/a b.rs")
        );
        assert_eq!(
            wsl().uri_to_host("file:///mnt/i/プロジェクト/a b#1.rs"),
            Some(PathBuf::from(r"I:\プロジェクト\a b#1.rs"))
        );
        assert_eq!(
            wsl()
                .to_client_uri(Path::new(r"I:\プロジェクト\a b#1.rs"))
                .as_deref(),
            Some("file:///mnt/i/プロジェクト/a b#1.rs")
        );
        // 素のパスも受ける。
        assert_eq!(wsl().uri_to_host("/mnt/i/x"), Some(PathBuf::from(r"I:\x")));
        assert_eq!(
            wsl().uri_to_host("file:///mnt/i/a%20b"),
            Some(PathBuf::from(r"I:\a%20b"))
        );
        assert_eq!(wsl().uri_to_host("file://localhost/mnt/i/x"), None);
        assert_eq!(wsl().uri_to_host("file:///mnt/i/a/../b"), None);
        assert_eq!(wsl().uri_to_host("file:///I:/x"), None);
    }

    #[test]
    fn identity_mapper_only_normalizes() {
        let identity = IdentityPathMapper;
        assert_eq!(
            identity.to_host("/home/u/./a//b.rs"),
            Some(PathBuf::from("/home/u/a/b.rs"))
        );
        // POSIX の名前としては正しいので、Windows 向けの制限はかけない。
        assert_eq!(
            identity.to_host(r"/home/u/a:b\c"),
            Some(PathBuf::from(r"/home/u/a:b\c"))
        );
        assert_eq!(identity.to_host("/home/u/../x"), None);
        assert_eq!(identity.to_host("home/u"), None);
        assert_eq!(
            identity.to_client(Path::new("/home/u/a.rs")).as_deref(),
            Some("/home/u/a.rs")
        );
        assert_eq!(identity.to_client(Path::new("/home/u/../a.rs")), None);
        assert_eq!(
            identity
                .to_client_uri(Path::new("/home/u/日本語 a.rs"))
                .as_deref(),
            Some("file:///home/u/日本語 a.rs")
        );
    }

    /// ワークツリー `…/proj` を持つセッションの規則を、両方の変換器で。
    fn sandboxes() -> [(SandboxPaths, &'static str, &'static str); 2] {
        [
            (
                SandboxPaths::new(Arc::new(wsl()), vec![PathBuf::from(r"I:\proj")]),
                "/mnt/i/proj",
                r"I:\other\secret.rs",
            ),
            (
                SandboxPaths::new(Arc::new(IdentityPathMapper), vec![PathBuf::from("/proj")]),
                "/proj",
                "/other/secret.rs",
            ),
        ]
    }

    #[test]
    fn confines_paths_to_the_worktree_roots() {
        for (paths, client_root, outside_host) in sandboxes() {
            let inside = paths
                .to_host(&format!("{client_root}/src/main.rs"))
                .expect("a path inside the worktree maps");
            assert_eq!(
                paths.to_client(&inside),
                Some(format!("{client_root}/src/main.rs"))
            );
            assert_eq!(
                paths.to_client_uri(&inside),
                Some(format!("file://{client_root}/src/main.rs"))
            );
            assert!(
                paths.to_host(client_root).is_ok(),
                "the root itself is inside"
            );
            assert!(
                paths.to_host(&format!("{client_root}/")).is_ok(),
                "a trailing separator names the root"
            );
            assert!(paths.to_host(&format!("file://{client_root}/a.rs")).is_ok());

            for outside in [
                // 綴りの先頭が同じだけの隣のディレクトリ。
                format!("{client_root}X/a.rs"),
                format!("{client_root}/../other/a.rs"),
                format!("{client_root}/src/../../other/a.rs"),
                "/etc/passwd".to_string(),
                "/mnt/i/other/a.rs".to_string(),
            ] {
                assert!(
                    paths.to_host(&outside).is_err(),
                    "{outside:?} is outside the worktree"
                );
            }

            assert_eq!(paths.to_client(Path::new(outside_host)), None);
            assert_eq!(paths.to_client_uri(Path::new(outside_host)), None);
        }
    }

    #[test]
    fn compares_drive_letters_and_wsl_shares_without_case() {
        let paths = SandboxPaths::new(
            Arc::new(wsl()),
            vec![
                PathBuf::from(r"i:\proj"),
                PathBuf::from(r"\\wsl$\Ubuntu\home\u\repo"),
            ],
        );
        assert!(paths.to_host("/mnt/i/proj/a.rs").is_ok());
        assert!(paths.to_host("/mnt/I/proj/a.rs").is_ok());
        assert!(paths.to_host("/home/u/repo/a.rs").is_ok());
        assert!(paths.contains(Path::new(r"\\?\I:\proj\a.rs")));
        assert!(
            paths.to_host("/mnt/i/Proj/a.rs").is_err(),
            "names below the root are compared exactly"
        );
        assert_eq!(
            ComparablePath::new(Path::new(r"\\wsl$\ubuntu\home"), PathStyle::Windows),
            ComparablePath::new(
                Path::new(r"\\wsl.localhost\Ubuntu\home"),
                PathStyle::Windows
            )
        );
        if cfg!(windows) {
            assert!(same_location(
                Path::new(r"\\wsl$\ubuntu\home"),
                Path::new(r"\\wsl.localhost\Ubuntu\home")
            ));
        }
    }

    #[test]
    fn treats_other_spellings_of_a_root_as_outside() {
        // 8.3 の短い名前は、同じフォルダーでも綴りの上では別の名前なので中に入れない。
        let paths = SandboxPaths::new(Arc::new(wsl()), vec![PathBuf::from(r"I:\project")]);
        assert_eq!(
            paths.client_path_to_host("/mnt/i/PROJEC~1/a.rs"),
            Err(SandboxPathError::OutsideWorkspace)
        );
        assert!(!paths.contains(Path::new(r"I:\PROJEC~1\a.rs")));
        // デバイスの名前空間から見た同じドライブも、別の綴りとして断る。
        for host_path in [
            r"\\.\I:\project\a.rs",
            "//?/I:/project/a.rs",
            r"\??\I:\project\a.rs",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\project\a.rs",
        ] {
            assert!(
                !paths.contains(Path::new(host_path)),
                "{host_path:?} is outside"
            );
        }
        assert_eq!(paths.to_client(Path::new(r"I:\project\a.rs::$DATA")), None);
    }

    #[test]
    fn splits_host_paths_at_their_worktree_root() {
        let paths = SandboxPaths::new(
            Arc::new(wsl()),
            vec![
                PathBuf::from(r"I:\proj\"),
                PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p"),
            ],
        );
        let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
        assert_eq!(
            paths.split_at_root(Path::new(r"i:\proj\src\a.rs")),
            Some((Path::new(r"I:\proj\"), names(&["src", "a.rs"])))
        );
        assert_eq!(
            paths.split_at_root(Path::new(r"I:\proj")),
            Some((Path::new(r"I:\proj\"), Vec::new()))
        );
        assert_eq!(
            paths.split_at_root(Path::new(r"\\wsl$\ubuntu\root\p\b.rs")),
            Some((
                Path::new(r"\\wsl.localhost\Ubuntu\root\p"),
                names(&["b.rs"])
            ))
        );
        assert_eq!(paths.split_at_root(Path::new(r"I:\projX\a.rs")), None);
        assert_eq!(paths.split_at_root(Path::new(r"I:\proj\..\x")), None);
    }

    #[test]
    fn builds_a_wsl_mapper_only_for_one_distro() {
        let mapper = |roots: &[&str], distro: Option<&str>| {
            let roots = roots.iter().map(PathBuf::from).collect::<Vec<_>>();
            WslPathMapper::for_worktree_roots(&roots, distro)
        };
        assert_eq!(
            mapper(&[r"I:\proj"], None).ok(),
            Some(WslPathMapper {
                distro: String::new()
            })
        );
        assert_eq!(mapper(&[r"I:\proj"], Some("Ubuntu")).ok(), Some(wsl()));
        assert_eq!(
            mapper(
                &[
                    r"I:\proj",
                    r"\\wsl$\ubuntu\root\p",
                    r"\\wsl.localhost\Ubuntu\q"
                ],
                Some("Ubuntu")
            )
            .ok(),
            Some(wsl())
        );
        // 断る理由はそれぞれ別の変種で返る (呼び出し側はこれを見て日本語の文を選ぶので、
        // どの状況がどの変種になるかまで決めておく)。
        for (roots, distro, reason) in [
            // 既定のディストリビューションの名前は分からないので、同じか確かめられない。
            (
                &[r"\\wsl.localhost\Ubuntu\root\p"][..],
                None,
                WslWorktreeRootsError::DefaultDistroUnknown {
                    project: "Ubuntu".to_string(),
                },
            ),
            (
                &[r"\\wsl.localhost\Ubuntu\root\p"][..],
                Some("Debian"),
                WslWorktreeRootsError::DistroMismatch {
                    project: "Ubuntu".to_string(),
                    sandbox: "Debian".to_string(),
                },
            ),
            (
                &[r"\\wsl.localhost\Ubuntu\a", r"\\wsl$\Debian\b"][..],
                Some("Ubuntu"),
                WslWorktreeRootsError::MultipleDistros {
                    first: "Ubuntu".to_string(),
                    second: "Debian".to_string(),
                },
            ),
            (
                &[r"I:\proj", r"\\server\share\p"][..],
                Some("Ubuntu"),
                WslWorktreeRootsError::NetworkShare(PathBuf::from(r"\\server\share\p")),
            ),
            (
                &[r"\\wsl.localhost\Ubuntu\mnt\c\proj"][..],
                Some("Ubuntu"),
                WslWorktreeRootsError::DriveSeenThroughWsl(PathBuf::from(
                    r"\\wsl.localhost\Ubuntu\mnt\c\proj",
                )),
            ),
            (
                &[r"\\.\C:\proj"][..],
                Some("Ubuntu"),
                WslWorktreeRootsError::NotAWindowsPath(PathBuf::from(r"\\.\C:\proj")),
            ),
            (
                &["relative"][..],
                Some("Ubuntu"),
                WslWorktreeRootsError::NotAWindowsPath(PathBuf::from("relative")),
            ),
            (
                &[r"I:\proj"][..],
                Some("a/b"),
                WslWorktreeRootsError::InvalidDistroName("a/b".to_string()),
            ),
        ] {
            assert_eq!(
                mapper(roots, distro),
                Err(reason),
                "{roots:?} in {distro:?} must be refused"
            );
        }
    }

    /// ADR-F49 決定 23: WSL の共有をプロジェクトとして開くと、Zed のワークツリーの `abs_path`
    /// は Windows の `canonicalize` が返す `\\?\UNC\…` の綴りになる (`dunce` が畳めるのは
    /// ドライブの `\\?\C:\…` だけ)。その綴りのルートでも変換器は組めて、サンドボックスから
    /// 来たパスは **ルートと同じ綴りで** 返る (Zed がワークツリーを見つけるのは前方一致なので、
    /// `\\wsl.localhost\…` のままでは同じ場所でもプロジェクトの外と判定される)。
    #[test]
    fn gives_back_the_spelling_of_a_canonicalized_wsl_root() {
        let root = r"\\?\UNC\wsl.localhost\Ubuntu\root\p";
        assert_eq!(
            WslPathMapper::for_worktree_roots(&[PathBuf::from(root)], Some("Ubuntu")).ok(),
            Some(wsl()),
            "a canonicalized root names the same distribution"
        );
        let paths = SandboxPaths::new(Arc::new(wsl()), vec![PathBuf::from(root)]);
        assert!(paths.contains(Path::new(r"\\wsl.localhost\Ubuntu\root\p\seed.txt")));
        assert_eq!(
            paths.client_path_to_host("/root/p/seed.txt"),
            Ok(PathBuf::from(format!(r"{root}\seed.txt")))
        );
        assert_eq!(
            paths.to_host("file:///root/p/sub/a.rs").ok(),
            Some(PathBuf::from(format!(r"{root}\sub\a.rs")))
        );
        assert_eq!(
            paths.client_path_to_host("/root/p"),
            Ok(PathBuf::from(root)),
            "the root itself keeps its own spelling too"
        );
        assert_eq!(
            paths.to_client(Path::new(&format!(r"{root}\seed.txt"))),
            Some("/root/p/seed.txt".to_string()),
            "and the way back reads that spelling"
        );
        assert_eq!(
            paths.client_path_to_host("/root/q/a.rs"),
            Err(SandboxPathError::OutsideWorkspace)
        );
    }

    /// ルートがどの綴りで書かれていても、返すパスはそのルートの綴りに合わせる。ドライブの
    /// ワークツリーは今までどおり (綴りは `canonicalize` を通しても `I:\…` のまま)。
    #[test]
    fn answers_in_the_spelling_of_each_root() {
        for (root, client_path, expected) in [
            (
                r"\\wsl$\Ubuntu\root\p",
                "/root/p/a.rs",
                r"\\wsl$\Ubuntu\root\p\a.rs",
            ),
            (
                r"\\wsl.localhost\Ubuntu\root\p",
                "/root/p/a.rs",
                r"\\wsl.localhost\Ubuntu\root\p\a.rs",
            ),
            (r"I:\proj", "/mnt/i/proj/a.rs", r"I:\proj\a.rs"),
            // 末尾の区切りがあっても重ねない。
            (r"I:\proj\", "/mnt/i/proj/a.rs", r"I:\proj\a.rs"),
            (r"I:\", "/mnt/i/a.rs", r"I:\a.rs"),
        ] {
            let paths = SandboxPaths::new(Arc::new(wsl()), vec![PathBuf::from(root)]);
            assert_eq!(
                paths.client_path_to_host(client_path),
                Ok(PathBuf::from(expected)),
                "{client_path} under {root}"
            );
        }
        // POSIX のホストでは区切りは `/` だけ。
        let paths = SandboxPaths::new(
            Arc::new(IdentityPathMapper),
            vec![PathBuf::from("/proj"), PathBuf::from("/other/")],
        );
        assert_eq!(
            paths.client_path_to_host("/proj/src/a.rs"),
            Ok(PathBuf::from("/proj/src/a.rs"))
        );
        assert_eq!(
            paths.client_path_to_host("/other/a.rs"),
            Ok(PathBuf::from("/other/a.rs"))
        );
    }

    fn two_worktrees() -> SandboxPaths {
        SandboxPaths::new(
            Arc::new(wsl()),
            vec![
                PathBuf::from(r"I:\proj"),
                PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p"),
            ],
        )
    }

    #[test]
    fn maps_client_paths_that_are_not_uris() {
        let paths = two_worktrees();
        assert_eq!(
            paths.client_path_to_host("/mnt/i/proj/a%20b.rs"),
            Ok(PathBuf::from(r"I:\proj\a%20b.rs"))
        );
        assert_eq!(
            paths.client_path_to_host("/mnt/i/proj"),
            Ok(PathBuf::from(r"I:\proj"))
        );
        assert_eq!(
            paths.client_path_to_host("/mnt/i/proj/"),
            Ok(PathBuf::from(r"I:\proj"))
        );
        assert_eq!(
            paths.client_path_to_host("/root/p/日本語 a.rs"),
            Ok(PathBuf::from(r"\\wsl.localhost\Ubuntu\root\p\日本語 a.rs"))
        );
        for client_path in [
            "file:///mnt/i/proj/a.rs",
            "/mnt/i/proj/../x",
            "/mnt/i/proj/a.rs::$DATA",
            " /mnt/i/proj/a.rs",
            "`/mnt/i/proj/a.rs`",
        ] {
            assert_eq!(
                paths.client_path_to_host(client_path),
                Err(SandboxPathError::Unmappable),
                "{client_path:?}"
            );
        }
        assert_eq!(
            paths.client_path_to_host("/mnt/i/other"),
            Err(SandboxPathError::OutsideWorkspace)
        );
    }

    #[test]
    fn decodes_client_file_uris_into_host_uris() {
        let paths = two_worktrees();
        for (client_uri, host_uri) in [
            (
                "file:///mnt/i/proj/src/main.rs",
                "file:///I:/proj/src/main.rs",
            ),
            (
                "file:///mnt/i/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E/a%20b.rs",
                "file:///I:/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E/a%20b.rs",
            ),
            // エンコードせずに送ってくる相手も、同じ名前として読む。
            (
                "file:///mnt/i/proj/日本語/a b.rs",
                "file:///I:/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E/a%20b.rs",
            ),
            (
                "file:///mnt/i/proj/100%25%20%23.txt",
                "file:///I:/proj/100%25%20%23.txt",
            ),
            (
                "file:///mnt/i/proj/a.rs#L10:20",
                "file:///I:/proj/a.rs#L10:20",
            ),
            (
                "file:///mnt/i/proj/a.rs?symbol=Foo#L1:2",
                "file:///I:/proj/a.rs?symbol=Foo#L1:2",
            ),
            ("file:///mnt/i/proj/src/", "file:///I:/proj/src/"),
            ("file:///mnt/i/proj", "file:///I:/proj"),
            ("file:///mnt/i/proj/", "file:///I:/proj/"),
            ("file:///mnt/I/proj/a.rs", "file:///I:/proj/a.rs"),
            ("FILE://localhost/mnt/i/proj/a.rs", "file:///I:/proj/a.rs"),
            (
                "file:///root/p/%E3%83%86%E3%82%B9%E3%83%88.rs",
                "file://wsl.localhost/Ubuntu/root/p/%E3%83%86%E3%82%B9%E3%83%88.rs",
            ),
            ("file:///root/p", "file://wsl.localhost/Ubuntu/root/p"),
        ] {
            assert_eq!(
                paths.client_file_uri_to_host_uri(client_uri).as_deref(),
                Ok(host_uri),
                "{client_uri}"
            );
        }
        for (client_uri, error) in [
            (
                "file:///mnt/i/other/a.rs",
                SandboxPathError::OutsideWorkspace,
            ),
            (
                "file:///mnt/i/projX/a.rs",
                SandboxPathError::OutsideWorkspace,
            ),
            (
                "file:///mnt/i/Proj/a.rs",
                SandboxPathError::OutsideWorkspace,
            ),
            ("file:///root/other", SandboxPathError::OutsideWorkspace),
            (
                "file:///mnt/i/proj/%2E%2E/other/a.rs",
                SandboxPathError::Unmappable,
            ),
            (
                "file:///mnt/i/proj/%2e%2e/other/a.rs",
                SandboxPathError::Unmappable,
            ),
            (
                "file:///mnt/i/proj/a%2F..%2F..%2Fother",
                SandboxPathError::Unmappable,
            ),
            (
                "file:///mnt/i/proj/a/../../other",
                SandboxPathError::Unmappable,
            ),
            // 冗長な UTF-8 (`%C0%AE` は `.` の 2 バイト表現) は UTF-8 として読めないので断る。
            (
                "file:///mnt/i/proj/%C0%AE%C0%AE/other",
                SandboxPathError::Unmappable,
            ),
            // Windows の名前には `?`・`\`・`:` を使えない。
            ("file:///mnt/i/proj/a%3F.rs", SandboxPathError::Unmappable),
            ("file:///mnt/i/proj/a%5Cb", SandboxPathError::Unmappable),
            (
                "file:///mnt/i/proj/a.rs::$DATA",
                SandboxPathError::Unmappable,
            ),
            ("file:///mnt/i/proj/%FF.rs", SandboxPathError::Unmappable),
            ("file:///mnt/i/proj/a%00b", SandboxPathError::Unmappable),
            (
                "file://server/mnt/i/proj/a.rs",
                SandboxPathError::Unmappable,
            ),
            ("file:/mnt/i/proj/a.rs", SandboxPathError::Unmappable),
            ("file:///I:/proj/a.rs", SandboxPathError::Unmappable),
            (" file:///mnt/i/proj/a.rs", SandboxPathError::Unmappable),
            ("\tfile:///mnt/i/proj/a.rs", SandboxPathError::Unmappable),
            ("`file:///mnt/i/proj/a.rs`", SandboxPathError::Unmappable),
            (
                "https://example.com/mnt/i/proj",
                SandboxPathError::Unmappable,
            ),
            ("/mnt/i/proj/a.rs", SandboxPathError::Unmappable),
        ] {
            assert_eq!(
                paths.client_file_uri_to_host_uri(client_uri),
                Err(error),
                "{client_uri:?}"
            );
        }
    }

    #[test]
    fn encodes_host_file_uris_for_the_client() {
        let paths = two_worktrees();
        for (host_uri, client_uri) in [
            (
                "file:///I:/proj/src/main.rs",
                "file:///mnt/i/proj/src/main.rs",
            ),
            // Zed のメンション (`url` の `set_path`) の形。
            (
                "file:///I:/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E%20a.rs",
                "file:///mnt/i/proj/%E6%97%A5%E6%9C%AC%E8%AA%9E%20a.rs",
            ),
            ("file:///i:/proj/a.rs", "file:///mnt/i/proj/a.rs"),
            ("file:///I%3A/proj/a.rs", "file:///mnt/i/proj/a.rs"),
            (
                "file:///I:/proj/100%25.txt?symbol=x#L3",
                "file:///mnt/i/proj/100%25.txt?symbol=x#L3",
            ),
            (
                "file:///I:/proj/a%23b.rs#L3",
                "file:///mnt/i/proj/a%23b.rs#L3",
            ),
            ("file:///I:/proj/src/", "file:///mnt/i/proj/src/"),
            ("file:///I:/proj/src%5C", "file:///mnt/i/proj/src/"),
            ("file:///I:/proj", "file:///mnt/i/proj"),
            (
                "file://wsl.localhost/Ubuntu/root/p/a.rs",
                "file:///root/p/a.rs",
            ),
            (
                "file:////wsl.localhost/Ubuntu/root/p/a%20b.rs",
                "file:///root/p/a%20b.rs",
            ),
            ("file://wsl$/ubuntu/root/p/", "file:///root/p/"),
        ] {
            assert_eq!(
                paths.host_file_uri_to_client_uri(host_uri).as_deref(),
                Ok(client_uri),
                "{host_uri}"
            );
        }
        for (host_uri, error) in [
            ("file:///I:/other/a.rs", SandboxPathError::OutsideWorkspace),
            (
                "file:///C:/Users/me/.ssh/id_rsa",
                SandboxPathError::OutsideWorkspace,
            ),
            (
                "file://wsl.localhost/Debian/root/p/a.rs",
                SandboxPathError::OutsideWorkspace,
            ),
            (
                "file://server/share/proj/a.rs",
                SandboxPathError::OutsideWorkspace,
            ),
            (
                "file:///I:/proj/../other/a.rs",
                SandboxPathError::Unmappable,
            ),
            ("file:///I:/proj/%2E%2E/other", SandboxPathError::Unmappable),
            ("file:///I:/proj/%2e%2e/other", SandboxPathError::Unmappable),
            (
                "file:///I:/proj/%C0%AE%C0%AE/other",
                SandboxPathError::Unmappable,
            ),
            ("file:///I:/proj/a.rs::$DATA", SandboxPathError::Unmappable),
            ("file:////?/I:/proj/a.rs", SandboxPathError::Unmappable),
            ("file:////./I:/proj/a.rs", SandboxPathError::Unmappable),
            ("file:///proj/a.rs", SandboxPathError::Unmappable),
            ("file:///I:proj/a.rs", SandboxPathError::Unmappable),
            (" file:///I:/proj/a.rs", SandboxPathError::Unmappable),
            ("zed:///agent/thread/1", SandboxPathError::Unmappable),
        ] {
            assert_eq!(
                paths.host_file_uri_to_client_uri(host_uri),
                Err(error),
                "{host_uri:?}"
            );
        }
    }

    #[test]
    fn round_trips_file_uris() {
        let paths = two_worktrees();
        for client_uri in [
            "file:///mnt/i/proj/%E6%97%A5%20a%25b%23c.rs#L1",
            "file:///mnt/i/proj/src/",
            "file:///mnt/i/proj",
            "file:///root/p/x.rs?symbol=main",
        ] {
            let host_uri = paths
                .client_file_uri_to_host_uri(client_uri)
                .unwrap_or_else(|error| panic!("{client_uri} maps to the host: {error}"));
            assert_eq!(
                paths.host_file_uri_to_client_uri(&host_uri).as_deref(),
                Ok(client_uri),
                "{client_uri} via {host_uri}"
            );
        }
    }

    #[test]
    fn maps_file_uris_for_an_identity_sandbox() {
        let paths = SandboxPaths::new(Arc::new(IdentityPathMapper), vec![PathBuf::from("/proj")]);
        assert_eq!(
            paths
                .client_file_uri_to_host_uri("file:///proj/%E6%97%A5%20a.rs")
                .as_deref(),
            Ok("file:///proj/%E6%97%A5%20a.rs")
        );
        // POSIX の名前には `\` を使える。末尾の `\` もディレクトリの印ではなく名前の一部。
        for uri in ["file:///proj/a%5Cb.rs", "file:///proj/a%5C"] {
            assert_eq!(
                paths.host_file_uri_to_client_uri(uri).as_deref(),
                Ok(uri),
                "{uri}"
            );
            assert_eq!(
                paths.client_file_uri_to_host_uri(uri).as_deref(),
                Ok(uri),
                "{uri}"
            );
        }
        assert_eq!(
            paths.host_file_uri_to_client_uri("file:///proj"),
            Ok("file:///proj".to_string())
        );
        assert_eq!(
            paths.client_file_uri_to_host_uri("file:///etc/passwd"),
            Err(SandboxPathError::OutsideWorkspace)
        );
        assert_eq!(
            paths.host_file_uri_to_client_uri("file://server/proj/a.rs"),
            Err(SandboxPathError::Unmappable)
        );
    }

    #[test]
    fn recognizes_wsl_share_paths() {
        for path in [
            r"\\wsl.localhost\Ubuntu",
            r"\\wsl.localhost\Ubuntu\home\user\project",
            r"\\WSL.LOCALHOST\Ubuntu\home",
            r"\\wsl$\Ubuntu-24.04\tmp\a.rs",
            r"\\?\UNC\wsl.localhost\Ubuntu\home\user",
            "//wsl.localhost/Ubuntu/home",
            // WSL 越しに見えるドライブも 9p の上。
            r"\\wsl.localhost\Ubuntu\mnt\c\work",
        ] {
            assert!(is_wsl_share_path(Path::new(path)), "{path}");
        }

        for path in [
            r"I:\zed-jp-llm\crates",
            r"\\?\I:\zed-jp-llm",
            r"\\server\share\project",
            // 共有の名前 (ディストリビューション) が無いものは場所ではない。
            r"\\wsl.localhost",
            // デバイスの名前空間はネットワークの共有ではない。
            r"\\.\wsl.localhost\Ubuntu",
            "/home/user/project",
            "",
        ] {
            assert!(!is_wsl_share_path(Path::new(path)), "{path}");
        }
    }

    /// ADR-F49 決定 34: 同じ場所を指す共有の綴りは 1 つに畳み、場所が変わりうるもの
    /// (別のサーバー・ドライブ・ルート自身・`..`) には触らない。
    #[test]
    fn folds_the_spellings_of_a_grant_in_a_distribution() {
        for (path, folded) in [
            (
                r"\\wsl.localhost\Ubuntu\home\u\repo",
                r"\\wsl.localhost\ubuntu\home\u\repo",
            ),
            (
                r"\\wsl$\Ubuntu\home\u\repo",
                r"\\wsl.localhost\ubuntu\home\u\repo",
            ),
            (
                r"\\?\UNC\wsl.localhost\UBUNTU\home\u\repo",
                r"\\wsl.localhost\ubuntu\home\u\repo",
            ),
            (
                "//wsl.localhost/Ubuntu/home/u/repo",
                r"\\wsl.localhost\ubuntu\home\u\repo",
            ),
            (
                // ext4 では名前の大文字小文字は別の場所。
                r"\\wsl.localhost\Ubuntu\home\U\Repo",
                r"\\wsl.localhost\ubuntu\home\U\Repo",
            ),
            (
                r"\\wsl.localhost\Ubuntu-24.04\tmp\x",
                r"\\wsl.localhost\ubuntu-24.04\tmp\x",
            ),
        ] {
            assert_eq!(
                canonical_wsl_grant_spelling(path).as_deref(),
                Some(folded),
                "{path}"
            );
        }

        for path in [
            r"I:\zed-jp-llm\crates",
            r"\\server\share\project",
            "/home/u/repo",
            // ディストリビューションのルート自身は書込許可の形ではない。
            r"\\wsl.localhost\Ubuntu",
            r"\\wsl.localhost\Ubuntu\\",
            // 畳んだ先が見せた場所とは限らないものは触らない。
            r"\\wsl.localhost\Ubuntu\home\..\root",
            "",
        ] {
            assert_eq!(canonical_wsl_grant_spelling(path), None, "{path}");
        }

        assert_eq!(
            canonical_wsl_grant_spelling(r"\\wsl.localhost\ubuntu\home\u\repo").as_deref(),
            Some(r"\\wsl.localhost\ubuntu\home\u\repo"),
            "畳んだ綴りは畳んでも変わらない"
        );
    }
}
