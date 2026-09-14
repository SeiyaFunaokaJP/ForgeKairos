//! サンドボックス (WSL) の中の CLI が指したパスについての、ファイルシステムに触れる確認
//! (ADR-F49 決定 4)。パスの変換とワークツリーの中かどうかの判定は [`util::path_mapping`]。

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::FileHandle;
use gpui::BackgroundExecutor;
use project::Fs;
use util::path_mapping::{ComparablePath, SandboxPaths};
use util::paths::PathStyle;

/// ワークツリーのルートより下の成分に、リンク (シンボリックリンク・ジャンクション・WSL の
/// リンク) や、`\\wsl.localhost` の特殊ファイルがあるか。WSL 側に作られたリンクは Windows の
/// `canonicalize` でも `read_link` でも解決できないので、実体の比較とは別に成分ごとに見る。
pub async fn has_link_below_root(
    paths: &SandboxPaths,
    host_path: &Path,
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
) -> Result<bool> {
    Ok(!links_below_root(paths, host_path, fs, executor)
        .await?
        .is_empty())
}

/// サンドボックスが指したホストのパス (綴りの上ではワークツリーの中) が、ワークツリーの中に
/// 置かれたリンクを通って外へ出ないことを確かめる (ADR-F49 決定 23)。外部エージェントの ACP の
/// fs の読み書きは、ホストの Zed がファイルに触る前にこれを通す (`agent_servers::acp_paths` は
/// 綴りしか見ない)。通れば `host_path` をそのまま返す。
///
/// - ルートより下の各段がリンクでなければ通す
/// - リンクを通るときは、リンクごとにたどった先がワークツリーのルート (の実体) の中なら通し、
///   外なら断る。たどれないリンク (壊れたリンクなど) は `canonicalize` が失敗するので断る
/// - WSL の中で作られたリンクは、Windows の `canonicalize` ではたどれないことがある。失敗すれば
///   断り、たどらずにリンクそのものの場所を返したとき (行き先が分からない) も断る。
///   `\\wsl.localhost` の下のリンクと Linux の特殊ファイルは、`canonicalize` の結果を信じられない
///   ので、行き先に関わらず断る
/// - 確かめてから使うまでのあいだにリンクへ差し替えられうる (サンドボックスはワークツリーに
///   書ける)。読むだけなら、1 つのハンドルで確かめてから読む [`RealRoots::read_file`] を使う
pub async fn resolve_inside_roots(
    paths: &SandboxPaths,
    host_path: &Path,
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
) -> Result<PathBuf> {
    let links = links_below_root(paths, host_path, fs, executor).await?;
    anyhow::ensure!(
        links.is_empty()
            || !paths
                .comparable(host_path)
                .is_some_and(|path| path.is_unc()),
        "the path goes through a link or a special file inside WSL, which Zed cannot follow"
    );
    let real_roots = real_roots(paths, fs).await;
    for link in &links {
        ensure_link_stays_inside(link, &real_roots, fs).await?;
    }
    real_roots.ensure_contains(host_path, fs).await?;
    Ok(host_path.to_path_buf())
}

/// ルートより下で、リンクか `\\wsl.localhost` の特殊ファイルである段 (ルートに近い順)。
async fn links_below_root(
    paths: &SandboxPaths,
    host_path: &Path,
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
) -> Result<Vec<PathBuf>> {
    let (root, components) = paths
        .split_at_root(host_path)
        .context("the path is outside the workspace")?;
    let mut prefix = root.to_path_buf();
    // リンクをたどらず、データも求めずに成分ごとの種類を見る。`Fs::metadata` はたどった先も
    // 開くので、`\\wsl.localhost` の FIFO ではリダイレクタが打ち切る 10 秒まで止まる。
    if !fs.is_fake() {
        return executor
            .spawn(async move {
                let mut links = Vec::new();
                for component in components {
                    prefix.push(component);
                    match std::fs::symlink_metadata(&prefix) {
                        Ok(metadata)
                            if metadata.file_type().is_symlink()
                                || is_link_or_special(&metadata, &prefix) =>
                        {
                            links.push(prefix.clone());
                        }
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) =>
                        {
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                anyhow::Ok(links)
            })
            .await;
    }
    // `FakeFs` はディスクに実体が無いので、`Fs` を通して同じ判定をする。
    let mut links = Vec::new();
    for component in components {
        prefix.push(component);
        // `read_link` が通るのはリンクだけ (壊れたリンクも含む)。失敗は「リンクではない」。
        if fs.read_link(&prefix).await.is_ok() {
            links.push(prefix.clone());
            continue;
        }
        match fs.metadata(&prefix).await? {
            Some(metadata) if metadata.is_symlink => links.push(prefix.clone()),
            Some(_) => {}
            None => break,
        }
    }
    Ok(links)
}

async fn ensure_link_stays_inside(
    link: &Path,
    real_roots: &RealRoots,
    fs: &Arc<dyn Fs>,
) -> Result<()> {
    let target = fs
        .canonicalize(link)
        .await
        .context("the link cannot be followed")?;
    let (Some(parent), Some(name)) = (link.parent(), link.file_name()) else {
        anyhow::bail!("the link has no parent folder");
    };
    let unfollowed = fs.canonicalize(parent).await?.join(name);
    anyhow::ensure!(
        !same_final_location(&target, &unfollowed),
        "Zed cannot follow the link"
    );
    real_roots
        .ensure_contains(&target, fs)
        .await
        .context("the link leads outside the workspace")
}

/// シンボリックリンクをたどったあとのワークツリーのルート (ルート自体がリンクでもよい)。
pub async fn real_roots(paths: &SandboxPaths, fs: &Arc<dyn Fs>) -> RealRoots {
    let mut roots = Vec::new();
    for root in paths.roots() {
        let Some(lexical) = paths.comparable(root) else {
            continue;
        };
        match real_location(root, fs).await {
            Ok(real_path) => {
                if let Some(real) = ComparablePath::local(&real_path) {
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
    RealRoots {
        roots,
        host_path_style: paths.host_path_style(),
    }
}

struct RealRoot {
    /// セッションに渡されたままの綴りのルート。
    lexical: ComparablePath,
    real_path: PathBuf,
    real: ComparablePath,
}

/// 要求 1 件ぶんの、ワークツリーのルートの実体。
pub struct RealRoots {
    roots: Vec<RealRoot>,
    /// セッションに渡されたままの綴りと比べるときの読み方。
    host_path_style: PathStyle,
}

impl RealRoots {
    /// ワークツリーの中に置かれたリンクを経由して、ワークツリーの外を読ませないための確認。
    /// サンドボックスはワークツリーに書けるので、リンクはいくらでも作れる。
    pub async fn ensure_contains(&self, host_path: &Path, fs: &Arc<dyn Fs>) -> Result<()> {
        let real_path = real_location(host_path, fs).await?;
        let inside = ComparablePath::local(&real_path).is_some_and(|candidate| {
            self.roots
                .iter()
                .any(|root| candidate.starts_with(&root.real))
        });
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
        let candidate = ComparablePath::new(host_path, self.host_path_style)?;
        let root = self
            .roots
            .iter()
            .find(|root| candidate.starts_with(&root.lexical))?;
        let mut expected = root.real_path.clone();
        expected.extend(
            candidate
                .components()
                .iter()
                .skip(root.lexical.components().len()),
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
    ComparablePath::local(path).is_some_and(|parsed| parsed.is_unc())
}

/// 開いたハンドルの実体の場所と、行き着くはずの場所の比較。ドライブ文字のパス (NTFS など) は
/// 大文字小文字を区別しないので、ディスク上の綴りと CLI の綴りの大小の違いは同じ場所とみなす
/// (同じディレクトリの中の話で、ワークツリーの外には出ない)。UNC (`\\wsl.localhost` など) の先は
/// 区別するファイルシステムでありうるので、綴りどおりに比べる。
fn same_final_location(left: &Path, right: &Path) -> bool {
    let (Some(left), Some(right)) = (ComparablePath::local(left), ComparablePath::local(right))
    else {
        return false;
    };
    if !cfg!(windows) || left.is_unc() {
        return left == right;
    }
    left.root() == right.root()
        && left.components().len() == right.components().len()
        && left
            .components()
            .iter()
            .zip(right.components())
            .all(|(left, right)| left.to_lowercase() == right.to_lowercase())
}

#[cfg(test)]
mod tests {
    use util::path_mapping::{PathMapper as _, WslPathMapper};

    /// ドライブ文字のパスは大文字小文字の違いを同じ場所とみなすが、UNC (`\\wsl.localhost`) の
    /// 先は綴りどおりに比べる。
    #[cfg(windows)]
    #[test]
    fn compares_final_paths_exactly_on_unc_shares() {
        let same = |left: &str, right: &str| {
            super::same_final_location(std::path::Path::new(left), std::path::Path::new(right))
        };
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

    /// CLI から来た URI を直したホストのパスは、診断の応答では CLI が比べられる形の URI になる。
    #[test]
    fn renders_a_mapped_cli_uri_as_a_host_file_uri() {
        let host_path = WslPathMapper {
            distro: "Ubuntu".to_string(),
        }
        .uri_to_host("file:///mnt/i/x/main.rs")
        .expect("the URI maps to the host");
        assert_eq!(crate::tools::file_uri(&host_path), "file:///I:/x/main.rs");
    }
}
