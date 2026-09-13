//! JSON-RPC 2.0 の封筒と、Claude Code CLI が IDE を発見するためのロックファイル。
//!
//! MCP の型 (`Tool` / `CallToolParams` / …) は `context_server::types` を再利用し、
//! ここには「あちらに `pub` で出ていないもの」だけを置く。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::BackgroundExecutor;
use serde::{Deserialize, Serialize};

/// JSON-RPC の識別子。数値と文字列の両方が来る。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

/// 受信メッセージ。要求 (`id` あり) と通知 (`id` なし) の両方を受ける。
#[derive(Debug, Deserialize)]
pub struct IncomingMessage {
    #[serde(default)]
    pub id: Option<RequestId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct SuccessResponse {
    jsonrpc: &'static str,
    id: RequestId,
    result: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    jsonrpc: &'static str,
    id: RequestId,
    error: ErrorObject,
}

#[derive(Debug, Serialize)]
struct ErrorObject {
    code: i32,
    message: String,
}

#[derive(Debug, Serialize)]
struct OutgoingNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: serde_json::Value,
}

pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
pub const PARSE_ERROR: i32 = -32700;

/// `serde_json::to_string` はここでは失敗しない (どの値も `serde_json::Value` 由来) が、
/// `.rules` で `unwrap()` を禁じているため、失敗時は JSON-RPC のエラー封筒を手書きする。
fn encode<T: Serialize>(value: &T, id: &RequestId) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        Err(error) => {
            log::error!("ide_server: failed to serialize a JSON-RPC response: {error}");
            let id = match serde_json::to_string(id) {
                Ok(id) => id,
                Err(_) => "null".to_string(),
            };
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{INTERNAL_ERROR},"message":"response serialization failed"}}}}"#
            )
        }
    }
}

pub fn success_response(id: RequestId, result: serde_json::Value) -> String {
    let response = SuccessResponse {
        jsonrpc: "2.0",
        id: id.clone(),
        result,
    };
    encode(&response, &id)
}

pub fn error_response(id: RequestId, code: i32, message: impl Into<String>) -> String {
    let response = ErrorResponse {
        jsonrpc: "2.0",
        id: id.clone(),
        error: ErrorObject {
            code,
            message: message.into(),
        },
    };
    encode(&response, &id)
}

pub fn notification(method: &str, params: serde_json::Value) -> String {
    let notification = OutgoingNotification {
        jsonrpc: "2.0",
        method,
        params,
    };
    encode(&notification, &RequestId::Int(0))
}

/// Claude Code CLI が読むロックファイルの中身。
///
/// CLI 2.1.261 の実装 (`Pen()`) はポート番号を**ファイル名**から取るので、JSON には
/// 入れない。`transport` が `"ws"` のときだけ `ws://host:port` に、そうでなければ
/// `http://host:port/sse` に繋ぎに来る。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LockFileContents {
    pub pid: u32,
    pub workspace_folders: Vec<String>,
    pub ide_name: String,
    pub transport: String,
    pub running_in_windows: bool,
    pub auth_token: String,
}

impl LockFileContents {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(&self.to_value()).context("serializing the Claude Code IDE lock file")
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "pid": self.pid,
            "workspaceFolders": self.workspace_folders,
            "ideName": self.ide_name,
            "transport": self.transport,
            "runningInWindows": self.running_in_windows,
            "authToken": self.auth_token,
        })
    }

    pub fn from_json(text: &str) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            #[serde(default)]
            pid: Option<u32>,
            #[serde(default)]
            workspace_folders: Vec<String>,
            #[serde(default)]
            ide_name: Option<String>,
            #[serde(default)]
            transport: Option<String>,
            #[serde(default)]
            running_in_windows: bool,
            #[serde(default)]
            auth_token: Option<String>,
        }

        let raw: Raw = serde_json::from_str(text).context("parsing a Claude Code IDE lock file")?;
        Ok(Self {
            pid: raw.pid.unwrap_or_default(),
            workspace_folders: raw.workspace_folders,
            ide_name: raw.ide_name.unwrap_or_default(),
            transport: raw.transport.unwrap_or_default(),
            running_in_windows: raw.running_in_windows,
            auth_token: raw.auth_token.unwrap_or_default(),
        })
    }
}

/// CLI が探すディレクトリ: `$CLAUDE_CONFIG_DIR/ide`、未設定なら `~/.claude/ide`。
pub fn lock_file_directory() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("ide"),
        _ => util::paths::home_dir().join(".claude").join("ide"),
    }
}

pub fn lock_file_path(directory: &Path, port: u16) -> PathBuf {
    directory.join(format!("{port}.lock"))
}

/// Windows の `FILE_FLAG_OPEN_REPARSE_POINT`。`windows` クレートに依存しないよう値で持つ。
#[cfg(windows)]
pub(crate) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// Windows の `ERROR_SHARING_VIOLATION`。
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
const LOCK_RENAME_ATTEMPTS: u32 = 5;
const LOCK_RENAME_BACKOFF: Duration = Duration::from_millis(50);

/// ロックファイルを書く。
///
/// ロックの置き場所は他のプロセスからも書けるので、`<port>.lock` の名前でシンボリック
/// リンクを仕込まれても、その先を上書きしてはいけない。新しい一時ファイルをリンクを
/// たどらずに作って書き、同じディレクトリの中で名前を付け替える (`rename` は宛先が
/// リンクでもリンクそのものを置き換え、既存の通常ファイルもそのまま置き換える)。
pub async fn write_lock_file(
    path: &Path,
    contents: &[u8],
    executor: &BackgroundExecutor,
) -> Result<()> {
    let directory = path
        .parent()
        .context("the lock file path has no directory")?;
    let file_name = path
        .file_name()
        .context("the lock file path has no file name")?;
    // CLI は `*.lock` だけを読むので、書きかけの一時ファイルはその形にしない。
    let temporary_path = directory.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = match write_new_file(&temporary_path, contents) {
        Ok(()) => rename_with_retry(&temporary_path, path, executor).await,
        Err(error) => Err(error),
    }
    .with_context(|| format!("writing {}", path.display()));
    if result.is_err()
        && let Err(error) = std::fs::remove_file(&temporary_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::error!(
            "ide_server: could not remove {}: {error}",
            temporary_path.display()
        );
    }
    result
}

/// Windows では、CLI やウイルス対策ソフトがロックファイルを開いている一瞬、置き換えが
/// 共有違反やアクセス拒否で失敗する。少し待てば通るので、間を空けて何度か試す。
async fn rename_with_retry(
    from: &Path,
    to: &Path,
    executor: &BackgroundExecutor,
) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < LOCK_RENAME_ATTEMPTS && is_transient_rename_error(&error) => {
                executor.timer(LOCK_RENAME_BACKOFF * attempt).await;
                attempt += 1;
            }
            Err(error) => {
                if is_transient_rename_error(&error) {
                    log::warn!(
                        "ide_server: could not replace {} after {attempt} attempts: {error}",
                        to.display()
                    );
                }
                return Err(error);
            }
        }
    }
}

fn is_transient_rename_error(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
        return true;
    }
    error.kind() == std::io::ErrorKind::PermissionDenied
}

fn write_new_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    // Unix の `create_new` は O_CREAT|O_EXCL で、最後の成分がシンボリックリンクなら
    // (壊れたリンクでも) 失敗する。
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // 認証トークンが入るので本人だけが読めるようにする。
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // CREATE_NEW だけでは、その名前が壊れたシンボリックリンクだとリンク先に
        // 新しく作ってしまう。
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)
}
