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
        /// A selection was to be formatted and no server offers
        /// `textDocument/rangeFormatting`, but one can format the whole file.
        /// Defaulted so a payload from a remote server that predates the field
        /// still decodes.
        #[serde(default)]
        only_whole_buffer_formatting: bool,
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
            FormatFailure::NoFormattingLanguageServer {
                requested,
                only_whole_buffer_formatting,
            } => match requested {
                Some(name) => {
                    format!("language server `{name}` is not running or cannot format this buffer")
                }
                None if *only_whole_buffer_formatting => {
                    "no running language server can format a selection in this buffer, \
                     though one can format the whole buffer"
                        .to_string()
                }
                None => "no running language server can format this buffer".to_string(),
            },
        }
    }

    /// The short line (in the display language) the status bar shows next to
    /// 「フォーマットに失敗しました:」. It has no room for install hints, and the
    /// store that keeps it does not know which host it is talking about.
    pub fn status_message(&self) -> String {
        match self {
            FormatFailure::ExternalCommandNotFound { command, .. } => {
                i18n::t!("Formatter `{command}` was not found")
            }
            FormatFailure::ExternalCommandFailed {
                command, exit_code, ..
            } => match exit_code {
                Some(code) => i18n::t!("Formatter `{command}` failed with exit code {code}"),
                None => i18n::t!("Formatter `{command}` terminated abnormally"),
            },
            FormatFailure::NoFormattingLanguageServer {
                requested,
                only_whole_buffer_formatting,
            } => match requested {
                Some(name) => i18n::t!("Language server `{name}` cannot format"),
                None if *only_whole_buffer_formatting => {
                    i18n::t!("No language server can format a selection").to_string()
                }
                None => i18n::t!("No language server can format this file").to_string(),
            },
        }
    }

    /// The message shown to the user when a formatting they asked for failed.
    /// `remote_host` is the name of the machine the project lives on, when it is
    /// not this one: the tool has to be installed *there*, which is the single
    /// most confusing thing about formatting over a remote connection.
    pub fn user_message(&self, remote_host: Option<&str>) -> String {
        match self {
            FormatFailure::ExternalCommandNotFound { command, searched } => match remote_host {
                // ADR-F39: `sudo apt install` is the wrong advice for
                // the hosts this fork is aimed at — it needs root (so
                // it affects every other account) and it needs the
                // remote to be online, which is exactly what it is not.
                Some(host)
                    if crate::lsp_store::dev_tools::tool_by_command(command)
                        .is_some_and(|tool| tool.can_install()) =>
                {
                    i18n::t!(
                        "Formatter `{command}` was not found. It is not installed on the remote \
                         host ({host}). \"Install on Remote\" in the notification installs it in \
                         the remote home directory without administrator rights."
                    )
                }
                Some(host) => i18n::t!(
                    "Formatter `{command}` was not found. It is not installed on the remote host \
                     ({host}). Put `{command}` in a directory on PATH on the remote host."
                ),
                None if searched.is_empty() => {
                    i18n::t!("Formatter `{command}` was not found. It is not on PATH.")
                }
                None => i18n::t!(
                    "Formatter `{command}` was not found. Searched these places and PATH: {places}.",
                    places = searched.join(", ")
                ),
            },
            FormatFailure::ExternalCommandFailed {
                command,
                exit_code,
                stderr,
            } => {
                let mut message = match exit_code {
                    Some(code) => i18n::t!("Formatter `{command}` failed with exit code {code}."),
                    None => i18n::t!("Formatter `{command}` terminated abnormally."),
                };
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    message.push('\n');
                    message.push_str(stderr);
                }
                message
            }
            FormatFailure::NoFormattingLanguageServer {
                requested,
                only_whole_buffer_formatting,
            } => match requested {
                Some(name) => i18n::t!(
                    "Language server `{name}` is not running or does not support formatting."
                ),
                // A server is running, so there is nothing to install on the
                // remote host; the user can format the whole file instead.
                None if *only_whole_buffer_formatting => i18n::t!(
                    "No running language server can format a selection in this file. The whole file can be formatted."
                )
                .to_string(),
                None => match remote_host {
                    Some(host) => i18n::t!(
                        "No language server can format this file. Install a language server on \
                         the remote host ({host})."
                    ),
                    None => i18n::t!("No language server can format this file.").to_string(),
                },
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
/// translated lead-in, because they are the failures nobody anticipated.
pub fn format_failure_message(error: &anyhow::Error, remote_host: Option<&str>) -> String {
    match FormatFailure::from_error(error) {
        Some(failure) => failure.user_message(remote_host),
        None => i18n::t!("Formatting failed: {error}", error = format!("{error:#}")),
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
            "Formatting failed: something else went wrong"
        );
    }

    #[test]
    fn test_messages_follow_the_display_language() {
        let _locale = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(
            not_found().user_message(Some("192.168.100.150")),
            "フォーマッタ `clang-format` が見つかりません。接続先 (192.168.100.150) にインストールされていません。通知の「リモートにインストール」から、管理者権限なしで接続先のホームディレクトリに導入できます。"
        );
        assert_eq!(
            not_found().user_message(None),
            "フォーマッタ `clang-format` が見つかりません。次の場所と PATH を探しました: C:\\Zed, C:\\Zed\\bin。"
        );
        assert_eq!(
            not_found().status_message(),
            "フォーマッタ `clang-format` が見つかりません"
        );
        let nothing = FormatFailure::NoFormattingLanguageServer {
            requested: None,
            only_whole_buffer_formatting: false,
        };
        assert_eq!(
            nothing.user_message(Some("host")),
            "このファイルをフォーマットできる言語サーバーがありません。接続先 (host) に言語サーバーをインストールしてください。"
        );
        assert_eq!(
            format_failure_message(&anyhow::anyhow!("boom"), None),
            "フォーマットに失敗しました: boom"
        );
    }

    #[test]
    fn test_user_message_points_at_the_remote_host() {
        let local = not_found().user_message(None);
        assert!(local.contains("clang-format"), "{local}");
        assert!(local.contains("C:\\Zed\\bin"), "{local}");

        let remote = not_found().user_message(Some("192.168.100.150"));
        assert!(remote.contains("192.168.100.150"), "{remote}");
        // ADR-F39: a catalogued tool is offered as a one-button install rather
        // than as a `sudo` command that needs root and a network the host may
        // not have.
        assert!(remote.contains("Install on Remote"), "{remote}");
        assert!(!remote.contains("sudo"), "{remote}");
        // The local search paths are meaningless for a project on another
        // machine, so they must not be offered as somewhere to look.
        assert!(!remote.contains("C:\\Zed"), "{remote}");
    }

    #[test]
    fn test_selection_only_failure_does_not_claim_nothing_can_format() {
        let whole_file_only = FormatFailure::NoFormattingLanguageServer {
            requested: None,
            only_whole_buffer_formatting: true,
        };
        let nothing = FormatFailure::NoFormattingLanguageServer {
            requested: None,
            only_whole_buffer_formatting: false,
        };
        let remote = whole_file_only.user_message(Some("192.168.100.150"));
        assert_eq!(
            remote,
            "No running language server can format a selection in this file. The whole file can be formatted."
        );
        // A server is already running there, so asking for one to be
        // installed on the host would send the user the wrong way.
        assert_ne!(remote, nothing.user_message(Some("192.168.100.150")));
        assert_eq!(
            whole_file_only.status_message(),
            "No language server can format a selection"
        );
        assert_ne!(whole_file_only.log_message(), nothing.log_message());
    }

    #[test]
    fn test_payload_without_whole_buffer_flag_still_decodes() {
        // What a remote server from before the flag existed sends.
        let payload = br#"{"kind":"no_formatting_language_server","requested":null}"#;
        let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        assert_eq!(
            FormatFailure::from_tag_value(&value),
            Some(FormatFailure::NoFormattingLanguageServer {
                requested: None,
                only_whole_buffer_formatting: false,
            })
        );

        let failure = FormatFailure::NoFormattingLanguageServer {
            requested: None,
            only_whole_buffer_formatting: true,
        };
        let received =
            rpc::RpcError::from_proto(&failure.clone().into_error().to_proto(), "FormatBuffers");
        assert_eq!(FormatFailure::from_error(&received), Some(failure.clone()));

        // What a client from before the flag existed makes of the new payload.
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum FormatFailureBeforeFlag {
            NoFormattingLanguageServer { requested: Option<String> },
        }
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(failure.tag_value())
            .expect("the tag is base64");
        assert_eq!(
            serde_json::from_slice::<FormatFailureBeforeFlag>(&decoded).ok(),
            Some(FormatFailureBeforeFlag::NoFormattingLanguageServer { requested: None })
        );
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
