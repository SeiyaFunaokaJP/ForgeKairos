use gpui_shared_string::SharedString;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LanguageServerStatusUpdate {
    Binary(BinaryStatus),
    Health(ServerHealth, Option<SharedString>),
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum ServerHealth {
    Ok,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinaryStatus {
    None,
    CheckingForUpdate,
    Downloading,
    Starting,
    Stopping,
    Stopped,
    Failed {
        error: String,
    },
    /// The binary could not be fetched because the fork's egress allowlist
    /// refused a host the download needs (ADR-F22, `DESIGN.md` §4).
    ///
    /// A separate variant rather than a field on `Failed` so that every
    /// consumer of a failure has to decide, at compile time, whether it wants
    /// to offer the consent prompt or merely report the error the way it
    /// always did. `hosts` is what the user would have to allow, already
    /// expanded to cover the redirect hops
    /// (`http_client::egress_denied_hosts`), and `error` is the same string
    /// `Failed` would have carried.
    EgressDenied {
        error: String,
        hosts: Vec<String>,
    },
}
