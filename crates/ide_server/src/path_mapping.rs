//! サンドボックス (WSL) の中の CLI が使うパスと、Zed (ホスト) のパスの相互変換
//! (ADR-F49 決定 4)。
//!
//! サンドボックスのセッションでは、CLI から来るパスを信用しない入力として扱う。変換
//! できないもの、`..` を含むもの、ワークツリーの外を指すものはすべて拒否し、逆向き
//! (ホスト → CLI) でもワークツリーの外のパスは CLI に見せない。

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::FileHandle;
use gpui::BackgroundExecutor;
use project::Fs;

/// CLI (クライアント) 側のパスとホストのパスの変換。
///
/// どちらの向きも、変換できない・`..` を含む・相手側で別の意味になるパスには `None`
/// を返す。ワークツリーの中かどうかはここでは見ない (それは [`SandboxPaths`] の役目)。
pub trait PathMapper: Send + Sync {
    fn to_host(&self, client_path: &str) -> Option<PathBuf>;
    fn to_client(&self, host_path: &Path) -> Option<String>;

    /// `file://` の URI も素のパスも受ける。CLI の `normalizeFileUri` に合わせて
    /// パーセントデコードはしない。
    fn uri_to_host(&self, client_uri: &str) -> Option<PathBuf> {
        self.to_host(client_uri.strip_prefix("file://").unwrap_or(client_uri))
    }

    fn to_client_uri(&self, host_path: &Path) -> Option<String> {
        self.to_client(host_path)
            .map(|client_path| format!("file://{client_path}"))
    }
}

/// Windows のホストと、WSL のディストリビューションの中の CLI。
///
/// - `/mnt/<drive>/…` ↔ `<DRIVE>:\…` (扱う自動マウント先は既定の `/mnt` だけ)
/// - それ以外のディストリビューション内のパス ↔ `\\wsl.localhost\<distro>\…`
///   (ホスト側の入力では `\\wsl$\<distro>\…` も受ける)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslPathMapper {
    pub distro: String,
}

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
                if !is_wsl_server(server) || !share.eq_ignore_ascii_case(&self.distro) {
                    return None;
                }
                String::new()
            }
        };
        for component in &parsed.components {
            client_path.push('/');
            client_path.push_str(component);
        }
        if client_path.is_empty() {
            client_path.push('/');
        }
        Some(client_path)
    }
}

/// ホストと CLI が同じパスを使う環境 (Windows 以外のサンドボックス)。
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NameRules {
    /// 名前はホストの Windows で解釈される。
    Windows,
    Posix,
}

/// CLI の絶対パスを成分に分ける。`.` と空の成分 (`//`) は落とし、上の階層を指す成分を
/// 含むものは `None`。
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
    let is_name = |part: &&str| !matches!(*part, "" | "." | "..");
    let server = parts.next().filter(is_name)?;
    let share = parts.next().filter(is_name)?;
    Some((
        WindowsRoot::Unc { server, share },
        parts.next().unwrap_or_default(),
    ))
}

/// ワークツリーの中かどうかを成分単位で比べるための形。
#[derive(Debug, PartialEq, Eq)]
struct ComparablePath {
    root: String,
    components: Vec<String>,
}

impl ComparablePath {
    /// ドライブ文字と UNC のサーバー名・共有名だけは大文字小文字を区別しない (CLI は
    /// ドライブを小文字で返し、`\\wsl$\` と `\\wsl.localhost\` は同じ場所)。その先の名前は
    /// ワークツリーのルートから作ったパスが同じ綴りで戻ってくるので、厳密に比べる。
    #[cfg(windows)]
    fn new(path: &Path) -> Option<Self> {
        let parsed = WindowsPath::parse(path.to_str()?)?;
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

    #[cfg(not(windows))]
    fn new(path: &Path) -> Option<Self> {
        use std::path::Component;

        if !path.is_absolute() {
            return None;
        }
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => components.push(name.to_str()?.to_string()),
                Component::ParentDir | Component::Prefix(_) => return None,
            }
        }
        Some(Self {
            root: "/".to_string(),
            components,
        })
    }

    fn starts_with(&self, root: &Self) -> bool {
        self.root == root.root && self.components.starts_with(&root.components)
    }
}

/// 2 つのホストのパスが (綴りの揺れを除いて) 同じ場所を指すか。
pub fn same_location(left: &Path, right: &Path) -> bool {
    match (ComparablePath::new(left), ComparablePath::new(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// サンドボックスのセッション 1 本ぶんのパスの規則: 変換器と、CLI に見せてよい
/// ワークツリーのルート (ホストのパス)。
pub struct SandboxPaths {
    mapper: Arc<dyn PathMapper>,
    roots: Vec<PathBuf>,
}

impl SandboxPaths {
    pub fn new(mapper: Arc<dyn PathMapper>, roots: Vec<PathBuf>) -> Self {
        Self { mapper, roots }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// CLI から来たパス (か `file://` URI) を、ワークツリーの中のホストのパスに直す。
    pub fn to_host(&self, client_path: &str) -> Result<PathBuf> {
        let host_path = self
            .mapper
            .uri_to_host(client_path)
            .with_context(|| format!("{client_path} is not a path Zed can open"))?;
        anyhow::ensure!(
            self.contains(&host_path),
            "{client_path} is outside the workspace"
        );
        Ok(host_path)
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

    pub fn contains(&self, host_path: &Path) -> bool {
        let Some(candidate) = ComparablePath::new(host_path) else {
            return false;
        };
        self.roots
            .iter()
            .filter_map(|root| ComparablePath::new(root))
            .any(|root| candidate.starts_with(&root))
    }

    /// ワークツリーのルートより下の成分に、リンク (シンボリックリンク・ジャンクション・WSL の
    /// リンク) や、`\\wsl.localhost` の特殊ファイルがあるか。WSL 側に作られたリンクは Windows の
    /// `canonicalize` でも `read_link` でも解決できないので、実体の比較とは別に成分ごとに見る。
    pub async fn has_link_below_root(
        &self,
        host_path: &Path,
        fs: &Arc<dyn Fs>,
        executor: &BackgroundExecutor,
    ) -> Result<bool> {
        let candidate =
            ComparablePath::new(host_path).context("the path cannot be compared with the roots")?;
        let (mut prefix, root_depth) = self
            .roots
            .iter()
            .find_map(|root| {
                let comparable = ComparablePath::new(root)?;
                candidate
                    .starts_with(&comparable)
                    .then(|| (root.clone(), comparable.components.len()))
            })
            .context("the path is outside the workspace")?;
        let components = candidate
            .components
            .into_iter()
            .skip(root_depth)
            .collect::<Vec<_>>();
        // リンクをたどらず、データも求めずに成分ごとの種類を見る。`Fs::metadata` はたどった先も
        // 開くので、`\\wsl.localhost` の FIFO ではリダイレクタが打ち切る 10 秒まで止まる。
        if !fs.is_fake() {
            return executor
                .spawn(async move {
                    for component in components {
                        prefix.push(component);
                        match std::fs::symlink_metadata(&prefix) {
                            Ok(metadata)
                                if metadata.file_type().is_symlink()
                                    || is_link_or_special(&metadata, &prefix) =>
                            {
                                return Ok(true);
                            }
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::NotFound
                                        | std::io::ErrorKind::NotADirectory
                                ) =>
                            {
                                break;
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    anyhow::Ok(false)
                })
                .await;
        }
        // `FakeFs` はディスクに実体が無いので、`Fs` を通して同じ判定をする。
        for component in components {
            prefix.push(component);
            // `read_link` が通るのはリンクだけ (壊れたリンクも含む)。失敗は「リンクではない」。
            if fs.read_link(&prefix).await.is_ok() {
                return Ok(true);
            }
            match fs.metadata(&prefix).await? {
                Some(metadata) if metadata.is_symlink => return Ok(true),
                Some(_) => {}
                None => break,
            }
        }
        Ok(false)
    }

    /// シンボリックリンクをたどったあとのワークツリーのルート (ルート自体がリンクでもよい)。
    pub async fn real_roots(&self, fs: &Arc<dyn Fs>) -> RealRoots {
        let mut roots = Vec::new();
        for root in &self.roots {
            let Some(lexical) = ComparablePath::new(root) else {
                continue;
            };
            match real_location(root, fs).await {
                Ok(real_path) => {
                    if let Some(real) = ComparablePath::new(&real_path) {
                        roots.push(RealRoot {
                            lexical,
                            real_path,
                            real,
                        });
                    }
                }
                Err(error) => log::warn!(
                    "ide_server: could not resolve the worktree {}: {error:#}",
                    root.display()
                ),
            }
        }
        RealRoots(roots)
    }
}

struct RealRoot {
    /// セッションに渡されたままの綴りのルート。
    lexical: ComparablePath,
    real_path: PathBuf,
    real: ComparablePath,
}

/// 要求 1 件ぶんの、ワークツリーのルートの実体。
pub struct RealRoots(Vec<RealRoot>);

impl RealRoots {
    /// ワークツリーの中に置かれたリンクを経由して、ワークツリーの外を読ませないための確認。
    /// サンドボックスはワークツリーに書けるので、リンクはいくらでも作れる。
    pub async fn ensure_contains(&self, host_path: &Path, fs: &Arc<dyn Fs>) -> Result<()> {
        let real_path = real_location(host_path, fs).await?;
        let inside = ComparablePath::new(&real_path)
            .is_some_and(|candidate| self.0.iter().any(|root| candidate.starts_with(&root.real)));
        anyhow::ensure!(
            inside,
            "the path resolves to a location outside the workspace"
        );
        Ok(())
    }

    /// ワークツリーの中のファイルを確かめてから読む。
    ///
    /// 確かめてからパスで開き直すと、そのあいだにリンクへ差し替えられうる (サンドボックスは
    /// ワークツリーに書ける) ので、1 つのハンドルを開き、その実体の場所と種類を確かめてから
    /// 同じハンドルで読む。大きさはメタデータを信じず、上限を 1 バイト超えるまで読んで決める
    /// (疎なファイルは読むあいだにも伸び縮みさせられる)。
    pub async fn read_file(
        &self,
        host_path: &Path,
        fs: &Arc<dyn Fs>,
        executor: &BackgroundExecutor,
    ) -> Result<SandboxFile> {
        let expected = self
            .expected_real_path(host_path)
            .context("the path is outside the workspace")?;
        // `FakeFs` はディスクに実体が無いので、テストでは `Fs` を通して同じ判定をする。
        if fs.is_fake() {
            return read_file_through_fs(host_path, fs).await;
        }
        let host_path = host_path.to_path_buf();
        let fs = fs.clone();
        executor
            .spawn(async move {
                read_file_through_handle(&host_path, &expected, MAX_SANDBOX_FILE_BYTES, &fs)
            })
            .await
    }

    /// ワークツリーの中のパスが、リンクをたどらずに行き着くはずの実体の場所。
    fn expected_real_path(&self, host_path: &Path) -> Option<PathBuf> {
        let candidate = ComparablePath::new(host_path)?;
        let root = self
            .0
            .iter()
            .find(|root| candidate.starts_with(&root.lexical))?;
        let mut expected = root.real_path.clone();
        expected.extend(
            candidate
                .components
                .iter()
                .skip(root.lexical.components.len()),
        );
        Some(expected)
    }
}

/// パスの実体。まだ無いファイル (`openDiff` の新規ファイルなど) は、存在する一番近い
/// 祖先の実体に残りの名前を足す。「まだ無い」とみなすのはメタデータが NotFound のとき
/// だけで、読めない・解決できない (壊れたリンクなど) ものは実体が分からないので失敗にする。
async fn real_location(path: &Path, fs: &Arc<dyn Fs>) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        if fs.metadata(&existing).await?.is_some() {
            let mut real_path = fs.canonicalize(&existing).await?;
            real_path.extend(missing.iter().rev());
            return Ok(real_path);
        }
        let name = existing
            .file_name()
            .context("the path has no existing ancestor")?
            .to_os_string();
        missing.push(name);
        existing.pop();
    }
}

/// サンドボックスの求めで読むファイル (`openDiff` の旧内容) の上限。
pub const MAX_SANDBOX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// サンドボックスの求めで読んだファイル。
#[derive(Debug, PartialEq, Eq)]
pub enum SandboxFile {
    /// まだ無い (`openDiff` の新しいファイル)。
    Missing,
    Text(String),
    /// ディレクトリ・FIFO・デバイス・ソケット・リンクなど、通常のファイルでないもの。
    NotRegular,
    TooLarge,
    /// 開いたものの実体が、リンクをたどらずに行き着くはずの場所と違った。
    Redirected,
}

async fn read_file_through_fs(host_path: &Path, fs: &Arc<dyn Fs>) -> Result<SandboxFile> {
    let Some(metadata) = fs.metadata(host_path).await? else {
        return Ok(SandboxFile::Missing);
    };
    if metadata.is_symlink {
        return Ok(SandboxFile::Redirected);
    }
    if metadata.is_dir || metadata.is_fifo {
        return Ok(SandboxFile::NotRegular);
    }
    let bytes = fs.load_bytes(host_path).await?;
    if u64::try_from(bytes.len())? > MAX_SANDBOX_FILE_BYTES {
        return Ok(SandboxFile::TooLarge);
    }
    Ok(SandboxFile::Text(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

/// 1 つのハンドルを開き、その実体の場所 (`expected` と同じか) と種類を確かめてから、同じ
/// ハンドルで `limit` を 1 バイト超えるまで読む。止まりうる OS の呼び出しをするので、
/// フォアグラウンドでは呼ばない。
pub(crate) fn read_file_through_handle(
    host_path: &Path,
    expected: &Path,
    limit: u64,
    fs: &Arc<dyn Fs>,
) -> Result<SandboxFile> {
    // `\\wsl.localhost` の FIFO は、読むために開くとリダイレクタが 10 秒で打ち切るまで戻らない
    // ので、データを求めずに開く `symlink_metadata` で種類を先に見る。そのあと開くまでに差し替え
    // られても、止まるのは打ち切りまでで、読むのは開いたハンドルで種類を確かめてから。
    #[cfg(windows)]
    if is_unc_path(host_path) {
        match std::fs::symlink_metadata(host_path) {
            Ok(metadata) if metadata.is_file() && !is_link_or_special(&metadata, host_path) => {}
            Ok(_) => return Ok(SandboxFile::NotRegular),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SandboxFile::Missing);
            }
            Err(error) => return Err(error.into()),
        }
    }
    let file = match open_for_reading(host_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SandboxFile::Missing);
        }
        // `O_NOFOLLOW` で開こうとした最後の成分がリンクだった。
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Ok(SandboxFile::Redirected);
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_link_or_special(&metadata, host_path) {
        return Ok(SandboxFile::NotRegular);
    }
    let current_path = FileHandle::current_path(&file, fs)?;
    if !same_final_location(&current_path, expected) {
        return Ok(SandboxFile::Redirected);
    }
    #[cfg(unix)]
    clear_nonblocking(&file)?;
    let mut bytes = Vec::new();
    (&file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > limit {
        return Ok(SandboxFile::TooLarge);
    }
    Ok(SandboxFile::Text(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

fn open_for_reading(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // FIFO を開くと、書き手が来るまで open が戻らない。最後の成分のリンクはたどらない
        // (途中の成分のリンクは、実体の場所の確認で断る)。端末を制御端末にしない。
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY);
    }
    #[cfg(windows)]
    if is_unc_path(path) {
        use std::os::windows::fs::OpenOptionsExt as _;
        // `\\wsl.localhost` では Linux の特殊ファイル (FIFO など) やリンクがリパースポイントに
        // 見える。たどらずに開き、種類の確認で断る。
        options.custom_flags(crate::protocol::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

/// 種類を確かめるまで付けておいた `O_NONBLOCK` を外す。ネットワークのファイルシステムには、
/// 通常のファイルでも `O_NONBLOCK` の読み込みを途中で `EAGAIN` にするものがある。
#[cfg(unix)]
fn clear_nonblocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let descriptor = file.as_raw_fd();
    // SAFETY: `descriptor` は `file` が持っていて、この呼び出しのあいだ開いている。
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: 同上。
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// リンク (名前の代理になるリパースポイント: シンボリックリンク・ジャンクション・WSL の
/// リンク) は断る。OneDrive のプレースホルダーのような、ほかのリパースポイントは通常の
/// ファイルとして読める。`\\wsl.localhost` のリパースポイントは Linux の特殊ファイルか
/// リンクなので、すべて断る。
#[cfg(windows)]
fn is_link_or_special(metadata: &std::fs::Metadata, path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || (is_unc_path(path) && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Unix では `is_file` が通れば通常のファイル。
#[cfg(not(windows))]
fn is_link_or_special(_metadata: &std::fs::Metadata, _path: &Path) -> bool {
    false
}

#[cfg(windows)]
fn is_unc_path(path: &Path) -> bool {
    path.to_str()
        .and_then(WindowsPath::parse)
        .is_some_and(|parsed| matches!(parsed.root, WindowsRoot::Unc { .. }))
}

/// 開いたハンドルの実体の場所と、行き着くはずの場所の比較。ドライブ文字のパス (NTFS など) は
/// 大文字小文字を区別しないので、ディスク上の綴りと CLI の綴りの大小の違いは同じ場所とみなす
/// (同じディレクトリの中の話で、ワークツリーの外には出ない)。UNC (`\\wsl.localhost` など) の先は
/// 区別するファイルシステムでありうるので、綴りどおりに比べる。
fn same_final_location(left: &Path, right: &Path) -> bool {
    let (Some(left), Some(right)) = (ComparablePath::new(left), ComparablePath::new(right)) else {
        return false;
    };
    if !cfg!(windows) || left.root.starts_with(r"\\") {
        return left == right;
    }
    left.root == right.root
        && left.components.len() == right.components.len()
        && left
            .components
            .iter()
            .zip(&right.components)
            .all(|(left, right)| left.to_lowercase() == right.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ドライブ文字のパスは大文字小文字の違いを同じ場所とみなすが、UNC (`\\wsl.localhost`) の
    /// 先は綴りどおりに比べる。
    #[cfg(windows)]
    #[test]
    fn compares_final_paths_exactly_on_unc_shares() {
        let same = |left: &str, right: &str| same_final_location(Path::new(left), Path::new(right));
        assert!(same(r"\\?\C:\Work\Main.rs", r"C:\work\main.rs"));
        assert!(same(
            r"\\?\UNC\wsl.localhost\Ubuntu\home\u\a.rs",
            r"\\wsl$\ubuntu\home\u\a.rs"
        ));
        assert!(!same(
            r"\\?\UNC\wsl.localhost\Ubuntu\home\u\A.rs",
            r"\\wsl.localhost\Ubuntu\home\u\a.rs"
        ));
        assert!(!same(r"C:\work\main.rs", r"C:\work\other\main.rs"));
    }

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

    #[test]
    fn maps_file_uris() {
        let host_path = wsl()
            .uri_to_host("file:///mnt/i/x/main.rs")
            .expect("the URI maps to the host");
        assert_eq!(crate::tools::file_uri(&host_path), "file:///I:/x/main.rs");
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
        // 素のパスも受ける。
        assert_eq!(wsl().uri_to_host("/mnt/i/x"), Some(PathBuf::from(r"I:\x")));
        // CLI と同じくパーセントデコードはしない。
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

    /// 実行している OS のパスで、ワークツリー `…/proj` を持つセッションの規則。
    fn sandbox_paths() -> (SandboxPaths, &'static str) {
        if cfg!(windows) {
            (
                SandboxPaths::new(Arc::new(wsl()), vec![PathBuf::from(r"I:\proj")]),
                "/mnt/i/proj",
            )
        } else {
            (
                SandboxPaths::new(Arc::new(IdentityPathMapper), vec![PathBuf::from("/proj")]),
                "/proj",
            )
        }
    }

    #[test]
    fn confines_paths_to_the_worktree_roots() {
        let (paths, client_root) = sandbox_paths();

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

        let outside_host = if cfg!(windows) {
            Path::new(r"I:\other\secret.rs")
        } else {
            Path::new("/other/secret.rs")
        };
        assert_eq!(paths.to_client(outside_host), None);
        assert_eq!(paths.to_client_uri(outside_host), None);
    }

    #[cfg(windows)]
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
        assert!(paths.to_host("/home/u/repo/a.rs").is_ok());
        assert!(paths.contains(Path::new(r"\\?\I:\proj\a.rs")));
        assert!(
            paths.to_host("/mnt/i/Proj/a.rs").is_err(),
            "names below the root are compared exactly"
        );
        assert!(same_location(
            Path::new(r"\\wsl$\ubuntu\home"),
            Path::new(r"\\wsl.localhost\Ubuntu\home")
        ));
    }
}
