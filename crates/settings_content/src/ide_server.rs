use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

/// Settings for the built-in IDE server (ADR-F26).
///
/// The server binds a random port on `127.0.0.1` only and never talks to the
/// network, so it is outside the scope of the egress allowlist (ADR-F03).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct IdeServerSettingsContent {
    /// Whether to expose this window over the loopback IDE server, so that
    /// external agents such as the Claude Code CLI can read the open editors,
    /// the selection and the diagnostics, and show diffs inside Zed.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// Whether to give agents Zed's IDE tools (the `zed` MCP server: highlights,
    /// annotations, code tours, hover, definitions, references and symbol
    /// search), in the Claude Code CLI tabs and in external agents' sessions
    /// alike. Independent of `enabled`: the server starts for them on demand
    /// and writes no lock file unless `enabled` is on.
    ///
    /// Default: true
    pub assist_tools: Option<bool>,
    /// Whether to tell a connected Claude Code CLI which file is open when
    /// nothing is selected (the CLI shows it as "In <file>" and attaches the
    /// file to the next message). When off, only a selection of one or more
    /// lines is sent.
    ///
    /// Default: true
    pub share_active_file: Option<bool>,
}
