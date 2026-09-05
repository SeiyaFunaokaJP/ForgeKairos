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
}
