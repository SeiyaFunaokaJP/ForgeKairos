//! JSON-RPC 2.0 の封筒と、Claude Code CLI が IDE を発見するためのロックファイル。
//!
//! MCP の型 (`Tool` / `CallToolParams` / …) は `context_server::types` を再利用し、
//! ここには「あちらに `pub` で出ていないもの」だけを置く。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
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
        serde_json::to_string(&serde_json::json!({
            "pid": self.pid,
            "workspaceFolders": self.workspace_folders,
            "ideName": self.ide_name,
            "transport": self.transport,
            "runningInWindows": self.running_in_windows,
            "authToken": self.auth_token,
        }))
        .context("serializing the Claude Code IDE lock file")
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
