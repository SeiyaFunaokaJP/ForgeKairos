//! Why formatting failed, in a form that survives the trip from wherever the
//! formatter ran to the window that has to explain it.
//!
//! Formatting happens on the machine that owns the files, so for a remote
//! project it happens inside `remote_server` and the failure comes back as a
//! `FormatBuffers` RPC error. That boundary flattens the whole `anyhow` chain
//! into one string, which would leave the client matching on English prose to
//! decide what to say. Instead the reason travels as a structured payload on an
//! [`rpc::RpcError`] tag, and only the client turns it into a sentence — it is
//! the side that knows whether the tool is missing locally or on the host the
//! user connected to.

use base64::Engine as _;
use rpc::{ErrorCode, ErrorCodeExt as _, ErrorExt as _};
use serde::{Deserialize, Serialize};

/// The `RpcError` tag key that [`FormatFailure`] rides on.
const FORMAT_FAILURE_TAG: &str = "format_failure";

/// How much of a failing formatter's `stderr` is worth carrying back to the
/// user. The first lines name the problem; the rest belongs in the log.
const MAX_STDERR_LINES: usize = 5;
const MAX_STDERR_CHARS: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormatFailure {
    /// The configured `formatter.external` command does not exist on the
    /// machine that formats. `searched` names the directories that were checked
    /// before `PATH`, so a bundled binary can be dropped in the right place.
    ExternalCommandNotFound {
        command: String,
        searched: Vec<String>,
    },
    /// The external formatter ran and rejected the buffer.
    ExternalCommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    /// A `language_server` formatter was configured, but no running server for
    /// this buffer offers `textDocument/formatting`.
    NoFormattingLanguageServer {
        /// The server the settings asked for by name, when they named one.
        requested: Option<String>,
    },
}

impl FormatFailure {
    /// Wraps this reason in the error type the RPC layer knows how to carry.
    /// The message stays English because it is what lands in the log on both
    /// sides; the localized text is built by [`Self::user_message`].
    pub fn into_error(self) -> anyhow::Error {
        let tag = self.tag_value();
        ErrorCode::Internal
            .message(self.log_message())
            .with_tag(FORMAT_FAILURE_TAG, &tag)
            .anyhow()
    }

    /// Recovers the reason from an error that came out of formatting, whether
    /// it was raised in this process or relayed by the remote server. Returns
    /// `None` for failures that carry no structured reason.
    pub fn from_error(error: &anyhow::Error) -> Option<Self> {
        Self::from_tag_value(error.error_tag(FORMAT_FAILURE_TAG)?)
    }

    fn tag_value(&self) -> String {
        let payload = match serde_json::to_vec(self) {
            Ok(payload) => payload,
            Err(error) => {
                // The error still travels, just without a reason the UI can
                // name, so there is nothing to propagate here.
                log::error!("failed to serialize a formatting failure: {error}");
                return String::new();
            }
        };
        // `RpcError::error_tag` splits tags on `=`, so the payload has to be
        // free of it: `stderr` is arbitrary program output and would otherwise
        // be truncated at the first `=` it happens to contain.
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    }

    fn from_tag_value(value: &str) -> Option<Self> {
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .ok()?;
        serde_json::from_slice(&decoded).ok()
    }

    /// The English one-liner for `~/Library/Logs/Zed/Zed.log` and friends.
    pub fn log_message(&self) -> String {
        match self {
            FormatFailure::ExternalCommandNotFound { command, searched } => {
                if searched.is_empty() {
                    format!("external formatter `{command}` was not found on PATH")
                } else {
                    format!(
                        "external formatter `{command}` was not found (searched {}, then PATH)",
                        searched.join(", ")
                    )
                }
            }
            FormatFailure::ExternalCommandFailed {
                command,
                exit_code,
                stderr,
            } => format!(
                "external formatter `{command}` failed with exit code {exit_code:?}: {stderr}"
            ),
            FormatFailure::NoFormattingLanguageServer { requested } => match requested {
                Some(name) => {
                    format!("language server `{name}` is not running or cannot format this buffer")
                }
                None => "no running language server can format this buffer".to_string(),
            },
        }
    }

    /// The short Japanese line the status bar shows next to
    /// 「フォーマットに失敗しました:」. It has no room for install hints, and the
    /// store that keeps it does not know which host it is talking about.
    pub fn status_message(&self) -> String {
        match self {
            FormatFailure::ExternalCommandNotFound { command, .. } => {
                format!("フォーマッタ `{command}` が見つかりません")
            }
            FormatFailure::ExternalCommandFailed {
                command, exit_code, ..
            } => match exit_code {
                Some(code) => {
                    format!("フォーマッタ `{command}` が終了コード {code} で失敗しました")
                }
                None => format!("フォーマッタ `{command}` が異常終了しました"),
            },
            FormatFailure::NoFormattingLanguageServer { requested } => match requested {
                Some(name) => format!("言語サーバー `{name}` はフォーマットに使えません"),
                None => "このファイルをフォーマットできる言語サーバーがありません".to_string(),
            },
        }
    }

    /// The message shown to the user when a formatting they asked for failed.
    /// `remote_host` is the name of the machine the project lives on, when it is
    /// not this one: the tool has to be installed *there*, which is the single
    /// most confusing thing about formatting over a remote connection.
    pub fn user_message(&self, remote_host: Option<&str>) -> String {
        match self {
            FormatFailure::ExternalCommandNotFound { command, searched } => {
                let mut message = format!("フォーマッタ `{command}` が見つかりません。");
                match remote_host {
                    Some(host) => {
                        message.push_str(&format!(
                            "接続先 ({host}) にインストールされていません。\
                             接続先で `{command}` をインストールしてください \
                             (Ubuntu/Debian なら `sudo apt install {command}`)。"
                        ));
                    }
                    None => {
                        if searched.is_empty() {
                            message.push_str("PATH から見つかりませんでした。");
                        } else {
                            message.push_str(&format!(
                                "次の場所と PATH を探しました: {}。",
                                searched.join(", ")
                            ));
                        }
                    }
                }
                message
            }
            FormatFailure::ExternalCommandFailed {
                command,
                exit_code,
                stderr,
            } => {
                let mut message = match exit_code {
                    Some(code) => {
                        format!("フォーマッタ `{command}` が終了コード {code} で失敗しました。")
                    }
                    None => format!("フォーマッタ `{command}` が異常終了しました。"),
                };
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    message.push('\n');
                    message.push_str(stderr);
                }
                message
            }
            FormatFailure::NoFormattingLanguageServer { requested } => match requested {
                Some(name) => format!(
                    "言語サーバー `{name}` が起動していないか、フォーマットに対応していません。"
                ),
                None => {
                    let mut message =
                        "このファイルをフォーマットできる言語サーバーがありません。".to_string();
                    if let Some(host) = remote_host {
                        message.push_str(&format!(
                            "接続先 ({host}) に言語サーバーをインストールしてください。"
                        ));
                    }
                    message
                }
            },
        }
    }
}

/// Trims a formatter's `stderr` down to what is worth showing and sending.
pub fn truncate_stderr(stderr: &str) -> String {
    let mut result = String::new();
    for line in stderr.trim().lines().take(MAX_STDERR_LINES) {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
        if result.chars().count() >= MAX_STDERR_CHARS {
            break;
        }
    }
    if result.chars().count() > MAX_STDERR_CHARS {
        result = result.chars().take(MAX_STDERR_CHARS).collect();
        result.push('…');
    }
    result
}

/// What to tell the user about a formatting failure, whether or not it carried a
/// structured reason. Errors without one keep their English detail after a
/// Japanese lead-in, because they are the failures nobody anticipated.
pub fn format_failure_message(error: &anyhow::Error, remote_host: Option<&str>) -> String {
    match FormatFailure::from_error(error) {
        Some(failure) => failure.user_message(remote_host),
        None => format!("フォーマットに失敗しました: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    fn not_found() -> FormatFailure {
        FormatFailure::ExternalCommandNotFound {
            command: "clang-format".to_string(),
            searched: vec!["C:\\Zed".to_string(), "C:\\Zed\\bin".to_string()],
        }
    }

    #[test]
    fn test_round_trip_through_anyhow() {
        let error = not_found().into_error();
        assert_eq!(FormatFailure::from_error(&error), Some(not_found()));
    }

    #[test]
    fn test_round_trip_survives_added_context() {
        // `apply_formatter` wraps the failure with the command it was running,
        // and the reason has to stay recoverable underneath that.
        let error = anyhow::Result::<()>::Err(not_found().into_error())
            .context("Failed to format buffer via external command: clang-format")
            .expect_err("the error was just constructed");
        assert_eq!(FormatFailure::from_error(&error), Some(not_found()));
    }

    #[test]
    fn test_round_trip_through_proto() {
        // The remote server answers `FormatBuffers` with `to_proto`, and the
        // client rebuilds the error with `RpcError::from_proto`.
        let failure = FormatFailure::ExternalCommandFailed {
            command: "clang-format".to_string(),
            // `=` in the payload would be eaten by the tag parser if the
            // encoding did not keep it out.
            stderr: "error: unknown argument: --style={Language: Cpp}".to_string(),
            exit_code: Some(1),
        };
        let sent = failure.clone().into_error().to_proto();
        let received = rpc::RpcError::from_proto(&sent, "FormatBuffers");
        assert_eq!(FormatFailure::from_error(&received), Some(failure));
    }

    #[test]
    fn test_unstructured_errors_have_no_reason() {
        let error = anyhow::anyhow!("something else went wrong");
        assert_eq!(FormatFailure::from_error(&error), None);
        assert_eq!(
            format_failure_message(&error, None),
            "フォーマットに失敗しました: something else went wrong"
        );
    }

    #[test]
    fn test_user_message_points_at_the_remote_host() {
        let local = not_found().user_message(None);
        assert!(local.contains("clang-format"), "{local}");
        assert!(local.contains("C:\\Zed\\bin"), "{local}");

        let remote = not_found().user_message(Some("192.168.100.150"));
        assert!(remote.contains("192.168.100.150"), "{remote}");
        assert!(remote.contains("sudo apt install clang-format"), "{remote}");
        // The local search paths are meaningless for a project on another
        // machine, so they must not be offered as somewhere to look.
        assert!(!remote.contains("C:\\Zed"), "{remote}");
    }

    #[test]
    fn test_truncate_stderr() {
        assert_eq!(truncate_stderr("  a\nb  "), "a\nb");
        let many_lines = (0..20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(truncate_stderr(&many_lines), "0\n1\n2\n3\n4");
        let long_line = "x".repeat(MAX_STDERR_CHARS * 2);
        let truncated = truncate_stderr(&long_line);
        assert_eq!(truncated.chars().count(), MAX_STDERR_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }
}
