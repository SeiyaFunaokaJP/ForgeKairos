//! Expect-style terminal macros and the saved connections that go with them.
//!
//! A macro is an ordered list of "send this line" / "wait until this text
//! appears" / "wait this long" steps, i.e. the subset of `pexpect` that router
//! and switch maintenance actually needs. Both macros and connections live in a
//! single user-owned JSON file (`terminal_sessions.json`) that is read with
//! comments tolerated and written atomically, the same way the egress policy
//! file is handled (DESIGN.md §4.3).

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use fs::Fs;
use futures::{StreamExt as _, future::LocalBoxFuture};
use futures_lite::future::yield_now;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Task};
use serde::{Deserialize, Serialize};

use crate::{Event, Terminal};

/// How many trailing lines of the grid a `WaitFor` step looks at. Large enough
/// for a paged `show running-config` banner, small enough that scanning it on
/// every wakeup stays free.
const WAIT_SCAN_LINES: usize = 200;

pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;

/// A step of a macro, as `terminal_sessions.json` spells it.
///
/// Fields this build does not know are refused rather than skipped: a step
/// carrying one was written by a later version, and a field added there can
/// change what the step does, so the step is kept as [`MacroStep::Unknown`]
/// instead of being run (or saved back) without it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MacroStep {
    /// Write `text` to the terminal, optionally followed by a carriage return
    /// (what a terminal emulator sends for Enter).
    Send {
        text: String,
        #[serde(default = "default_true")]
        newline: bool,
    },
    /// Block until `pattern` shows up in the terminal's new output.
    WaitFor {
        pattern: String,
        #[serde(default)]
        regex: bool,
        #[serde(default = "default_wait_timeout_ms")]
        timeout_ms: u64,
        /// Name of the variable the match is stored under, for later
        /// `${name}` references. A regex pattern also stores every named
        /// group of the match under its own name, whether or not this is set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capture: Option<String>,
        /// What to run instead of aborting the macro when `pattern` never
        /// turns up. `None` keeps the original behaviour (the run stops with
        /// an error); `Some` — even an empty list — means "carry on".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_timeout: Option<Vec<MacroStep>>,
    },
    /// Block for a fixed duration.
    Sleep { ms: u64 },
    /// Wipe the screen and the scrollback, the way `terminal::Clear` does.
    ///
    /// Braced rather than a unit variant because serde reads a tagged unit
    /// variant without looking at the other keys, so `deny_unknown_fields`
    /// would not see a field a later version added to it.
    Clear {},
    /// Run `steps` `count` times before moving on. `0` repeats until the macro
    /// is cancelled, the same rule as [`TerminalMacro::repeat`].
    Repeat {
        #[serde(default = "default_repeat")]
        count: u32,
        #[serde(default)]
        steps: Vec<MacroStep>,
    },
    /// Run `then_steps` when `pattern` is already in the output, `else_steps`
    /// when it is not.
    ///
    /// Unlike [`MacroStep::WaitFor`] this never waits: it decides on whatever
    /// the terminal holds at that moment, so a macro that needs the device to
    /// answer first puts a wait in front of the branch.
    If {
        pattern: String,
        #[serde(default)]
        regex: bool,
        #[serde(default, skip_serializing_if = "MatchScope::is_default")]
        scope: MatchScope,
        /// Name of the variable the match is stored under when `pattern` is
        /// found (「そのとき」 runs), the same rule [`MacroStep::WaitFor::capture`]
        /// follows. A regex pattern also stores every named group of the
        /// match under its own name, whether or not this is set. Nothing is
        /// stored when `pattern` is not found and 「それ以外」 runs instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capture: Option<String>,
        #[serde(rename = "then", default, skip_serializing_if = "Vec::is_empty")]
        then_steps: Vec<MacroStep>,
        #[serde(rename = "else", default, skip_serializing_if = "Vec::is_empty")]
        else_steps: Vec<MacroStep>,
    },
    /// Store `value`, with `${name}` references already expanded, under `name`
    /// for later steps. The only way to give a variable a value that did not
    /// come off the terminal: a host name typed once at the top, or two
    /// captures joined into one.
    SetVariable {
        name: String,
        #[serde(default)]
        value: String,
    },
    /// A step this build cannot read: a kind a later version added, a field
    /// it added to a known kind, or a value of a shape this build does not
    /// expect. Only this step is set aside, so the rest of the file (and of the
    /// macro) still loads, and it is written back exactly as it was read, so
    /// saving a macro here does not throw away what the later version wrote.
    /// A macro holding one is not run: skipping a step of a router
    /// maintenance script is not a safe guess.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// What a [`MacroStep::If`] looks at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchScope {
    /// Only what arrived after the last 送信, the same window a
    /// [`MacroStep::WaitFor`] measures. This is the default because a branch
    /// almost always asks about the answer to the command just sent, and the
    /// prompt that provoked it is still on screen.
    #[default]
    SinceLastSend,
    /// The trailing [`WAIT_SCAN_LINES`] lines of the grid, banner included.
    Screen,
    /// The value of the named variable. A variable nothing has set yet
    /// matches nothing, so the branch takes 「それ以外」; one set to the empty
    /// string is still tested, so `^$` can ask whether it came back empty.
    Variable(String),
}

impl MatchScope {
    fn is_default(&self) -> bool {
        matches!(self, MatchScope::SinceLastSend)
    }

    pub fn label(&self) -> &'static str {
        match self {
            MatchScope::SinceLastSend => i18n::t!("Output since last send"),
            MatchScope::Screen => i18n::t!("Whole screen"),
            MatchScope::Variable(_) => i18n::tc!("terminal macro", "Variable"),
        }
    }
}

/// Which branch of a [`MacroStep::If`] or a timed-out [`MacroStep::WaitFor`] is
/// running, so the session strip can say which way the macro went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroBranch {
    Then,
    Else,
    Timeout,
}

impl MacroBranch {
    pub fn label(&self) -> &'static str {
        match self {
            MacroBranch::Then => i18n::t!("Match → Then"),
            MacroBranch::Else => i18n::t!("No match → Else"),
            MacroBranch::Timeout => i18n::t!("Timeout → Continue"),
        }
    }

    /// Which [`MacroStepList`] this branch's steps sit in, for
    /// [`MacroRunner::step_label`].
    fn step_list(&self) -> MacroStepList {
        match self {
            MacroBranch::Then => MacroStepList::Main,
            MacroBranch::Else => MacroStepList::Else,
            MacroBranch::Timeout => MacroStepList::Timeout,
        }
    }
}

/// Which child list of a step a position in [`MacroRunner::path`] sits in:
/// the macro's own steps (also what a [`MacroStep::Repeat`] body and a
/// [`MacroStep::If`]'s 「そのとき」 side are made of), the 「それ以外」 side of
/// an `If`, or the 「タイムアウト時」 steps of a `WaitFor`.
///
/// [`MacroRunner::step_label`] spells a position by joining, with `-`, one
/// token per level: [`Self::prefix`] followed by the 1-based index. Only
/// `Else` and `Timeout` carry a prefix, which is what keeps 「そのとき」 and
/// 「それ以外」 from sharing a step number when they hold the same number of
/// steps — the editor's `StepPath::label` in `terminal_view` spells positions
/// the same way, so a run's error message and the row the editor highlights
/// agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroStepList {
    Main,
    Else,
    Timeout,
}

impl MacroStepList {
    /// Public so the macro editor in `terminal_view` spells a position from
    /// this one table instead of keeping a second copy of it.
    pub fn prefix(&self) -> &'static str {
        match self {
            MacroStepList::Main => "",
            MacroStepList::Else => "e",
            MacroStepList::Timeout => "t",
        }
    }
}

impl MacroStep {
    /// Label for the step kind, used by the editor's type dropdown.
    pub fn kind_label(&self) -> &'static str {
        match self {
            MacroStep::Send { .. } => i18n::tc!("terminal macro step", "Send"),
            MacroStep::WaitFor { .. } => i18n::tc!("terminal macro step", "Wait for Text"),
            MacroStep::Sleep { .. } => i18n::tc!("terminal macro step", "Sleep"),
            MacroStep::Clear {} => i18n::tc!("terminal macro step", "Clear Screen"),
            MacroStep::Repeat { .. } => i18n::tc!("terminal macro step", "Repeat"),
            MacroStep::If { .. } => i18n::tc!("terminal macro step", "Branch"),
            MacroStep::SetVariable { .. } => i18n::tc!("terminal macro step", "Set Variable"),
            MacroStep::Unknown(_) => i18n::tc!("terminal macro step", "Unknown Step"),
        }
    }

    /// The `type` a [`MacroStep::Unknown`] was written with, when it has one.
    pub fn unknown_kind(&self) -> Option<&str> {
        match self {
            MacroStep::Unknown(value) => value.get("type").and_then(|kind| kind.as_str()),
            _ => None,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            MacroStep::Send { text, .. } => i18n::t!("Send: {text}"),
            MacroStep::WaitFor {
                pattern,
                timeout_ms,
                capture,
                on_timeout,
                ..
            } => {
                let mut summary = i18n::t!(
                    "Wait for “{pattern}” (up to {} s)",
                    *timeout_ms as f64 / 1000.0
                );
                if let Some(capture) = capture {
                    summary.push_str(&i18n::t!(", capture into ${{{capture}}}"));
                }
                if let Some(on_timeout) = on_timeout {
                    summary.push_str(&i18n::tn!(
                        on_timeout.len(),
                        ", {count} step on timeout",
                        ", {count} steps on timeout"
                    ));
                }
                summary
            }
            MacroStep::Sleep { ms } => i18n::t!("Sleep: {} s", *ms as f64 / 1000.0),
            MacroStep::Clear {} => i18n::t!("Clear screen").to_string(),
            MacroStep::Repeat { count, steps } => match count {
                0 => i18n::tn!(
                    steps.len(),
                    "Repeat: until stopped ({count} step)",
                    "Repeat: until stopped ({count} steps)"
                ),
                times => i18n::tn!(
                    steps.len(),
                    "Repeat: ×{times} ({count} step)",
                    "Repeat: ×{times} ({count} steps)"
                ),
            },
            MacroStep::If {
                pattern,
                scope,
                capture,
                then_steps,
                else_steps,
                ..
            } => {
                let target = match scope {
                    MatchScope::Variable(name) => format!("${{{name}}}"),
                    MatchScope::SinceLastSend => {
                        i18n::t!("the output since the last send").to_string()
                    }
                    MatchScope::Screen => i18n::t!("the whole screen").to_string(),
                };
                let mut summary = i18n::t!(
                    "Branch: if “{pattern}” is in {target}, run {} step(s), otherwise {} step(s)",
                    then_steps.len(),
                    else_steps.len(),
                );
                if let Some(capture) = capture {
                    summary.push_str(&i18n::t!(", on a match capture into ${{{capture}}}"));
                }
                summary
            }
            MacroStep::SetVariable { name, value } => {
                i18n::t!("Variable: ${{{name}}} = “{value}”")
            }
            MacroStep::Unknown(_) => match self.unknown_kind() {
                Some(kind) => i18n::t!("Unknown step “{kind}” (this version can't run it)"),
                None => i18n::t!("Unknown step (this version can't run it)").to_string(),
            },
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

fn default_repeat() -> u32 {
    1
}

fn is_default_repeat(repeat: &u32) -> bool {
    *repeat == default_repeat()
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Identity of a macro. Two macros may carry the same name as long as they sit
/// in different folders (「A-PC用/ls」 and 「B-PC用/ls」 are the case this exists
/// for), so every lookup, reference and edit takes the pair rather than the
/// bare name.
pub type MacroKey = (Option<String>, String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalMacro {
    pub name: String,
    /// Which [`MacroFolder`] the macro is filed under, by name. `None` is
    /// 未分類. Normalized by [`normalize_folder`]; a value that names no folder
    /// in [`TerminalSessionsFile::macro_folders`] is repaired on load by
    /// [`TerminalSessionsFile::ensure_macro_folders`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// How many times the whole step list runs. `0` means "until cancelled".
    #[serde(default = "default_repeat", skip_serializing_if = "is_default_repeat")]
    pub repeat: u32,
    /// Pause between repetitions.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub repeat_interval_ms: u64,
    #[serde(default)]
    pub steps: Vec<MacroStep>,
}

impl Default for TerminalMacro {
    fn default() -> Self {
        Self {
            name: String::new(),
            folder: None,
            tags: Vec::new(),
            description: String::new(),
            repeat: default_repeat(),
            repeat_interval_ms: 0,
            steps: Vec::new(),
        }
    }
}

impl TerminalMacro {
    /// How many times the steps run, or `None` when the macro repeats until it
    /// is cancelled.
    pub fn iteration_count(&self) -> Option<usize> {
        (self.repeat > 0).then_some(self.repeat as usize)
    }

    pub fn folder_display(&self) -> &str {
        self.folder.as_deref().unwrap_or_default()
    }

    /// What identifies this macro among all saved ones.
    pub fn key(&self) -> MacroKey {
        (self.folder.clone(), self.name.clone())
    }

    pub fn is(&self, folder: Option<&str>, name: &str) -> bool {
        self.folder.as_deref() == folder && self.name == name
    }

    /// `フォルダー/名前`, which is what both the picker's search and its sort
    /// order work on.
    pub fn qualified_name(&self) -> String {
        match &self.folder {
            Some(folder) => format!("{folder}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// A grouping the user made in the macro editor (ADR-F35). Folders are first
/// class: one exists as soon as it is created, whether or not anything is filed
/// under it, and the order of [`TerminalSessionsFile::macro_folders`] is the
/// order the editor tree and the runner picker show — there is no separate
/// `order` field to keep in step with it.
///
/// The name doubles as the key [`TerminalMacro::folder`] and
/// [`SavedConnection::macro_folder`] point at, which is why renaming one has to
/// rewrite those too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroFolder {
    pub name: String,
    /// Whether the heading is folded shut. Kept in the file so the editor and
    /// the picker agree, and so the state survives closing the modal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsed: bool,
}

impl MacroFolder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            collapsed: false,
        }
    }
}

/// Normalizes a `/`-separated folder path: every segment is trimmed and empty
/// segments are dropped, so `" 拠点 / 本社 /"` and `"拠点/本社"` are the same
/// folder. An empty result is `None`, which keeps the key out of the JSON.
pub fn normalize_folder(folder: &str) -> Option<String> {
    let normalized = folder
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    (!normalized.is_empty()).then_some(normalized)
}

/// Splits a comma-separated tag field into trimmed, de-duplicated tags.
pub fn normalize_tags(tags: &str) -> Vec<String> {
    let mut normalized = Vec::new();
    for tag in tags.split(',') {
        let tag = tag.trim();
        if !tag.is_empty() && !normalized.iter().any(|existing| existing == tag) {
            normalized.push(tag.to_string());
        }
    }
    normalized
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionProtocol {
    #[default]
    Ssh,
    Telnet,
}

impl ConnectionProtocol {
    pub fn program(&self) -> &'static str {
        match self {
            ConnectionProtocol::Ssh => "ssh",
            ConnectionProtocol::Telnet => "telnet",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            ConnectionProtocol::Ssh => 22,
            ConnectionProtocol::Telnet => 23,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ConnectionProtocol::Ssh => "SSH",
            ConnectionProtocol::Telnet => "Telnet",
        }
    }

    /// Scheme used to key remembered passwords. Kept apart from
    /// [`Self::program`] so renaming the command never moves the credentials.
    pub fn scheme(&self) -> &'static str {
        match self {
            ConnectionProtocol::Ssh => "ssh",
            ConnectionProtocol::Telnet => "telnet",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    /// Same grouping path as [`TerminalMacro::folder`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default)]
    pub protocol: ConnectionProtocol,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Name of the macro to run once the terminal is up.
    #[serde(
        rename = "macro",
        default,
        skip_serializing_if = "Option::is_none",
        alias = "macro_name"
    )]
    pub macro_name: Option<String>,
    /// Folder half of the reference to that macro (ADR-F35), because the same
    /// name may exist in several folders. Absent in files written before
    /// folders became first class, where the name alone is resolved against the
    /// first macro carrying it — see
    /// [`TerminalSessionsFile::macro_for_connection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macro_folder: Option<String>,
    /// Whether to start a session log for this connection. `None` follows the
    /// `terminal.session_log.auto` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<bool>,
    /// Whether a password for this target is kept in the OS credential store
    /// (ADR-F28). Only the flag lives in `terminal_sessions.json`; the password
    /// itself never does — see [`crate::quick_connect_credentials`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remember_password: bool,
    /// Which file transfer protocol the transfer screen offers for this target
    /// (`sftp` / `scp` / `ftp` / `ftps`, ADR-F28), so one saved entry serves
    /// both the terminal and the transfer screen. A string rather than an enum
    /// because the protocols are defined in `file_transfer`, which depends on
    /// this crate; an unreadable value falls back to the protocol implied by
    /// [`Self::protocol`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_protocol: Option<String>,
    /// The port the transfer screen uses, when it is not the terminal's. An
    /// FTP service on a device the terminal reaches over Telnet is the case
    /// this exists for; SFTP and SCP share the SSH port and leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_port: Option<u16>,
}

impl SavedConnection {
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or_else(|| self.protocol.default_port())
    }

    /// The command line this connection spawns.
    pub fn command(&self) -> (String, Vec<String>) {
        let port = self.port_or_default();
        match self.protocol {
            ConnectionProtocol::Ssh => {
                let target = match self
                    .user
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                {
                    Some(user) => format!("{user}@{}", self.host),
                    None => self.host.clone(),
                };
                (
                    "ssh".to_string(),
                    vec!["-p".to_string(), port.to_string(), target],
                )
            }
            ConnectionProtocol::Telnet => (
                "telnet".to_string(),
                vec![self.host.clone(), port.to_string()],
            ),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSessionsFile {
    #[serde(default)]
    pub connections: Vec<SavedConnection>,
    /// The macro folders, in the order they are shown. Left out of the file
    /// entirely when there are none, so a file written before ADR-F35 comes
    /// back byte for byte until the user edits something.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macro_folders: Vec<MacroFolder>,
    #[serde(default)]
    pub macros: Vec<TerminalMacro>,
}

impl TerminalSessionsFile {
    pub fn macro_named(&self, folder: Option<&str>, name: &str) -> Option<&TerminalMacro> {
        self.macros
            .iter()
            .find(|terminal_macro| terminal_macro.is(folder, name))
    }

    pub fn macros_in_folder(&self, folder: Option<&str>) -> impl Iterator<Item = &TerminalMacro> {
        self.macros
            .iter()
            .filter(move |terminal_macro| terminal_macro.folder.as_deref() == folder)
    }

    pub fn folder_index(&self, name: &str) -> Option<usize> {
        self.macro_folders
            .iter()
            .position(|folder| folder.name == name)
    }

    pub fn has_folder(&self, name: &str) -> bool {
        self.folder_index(name).is_some()
    }

    /// Where a macro's folder sits in the display order. 未分類 (and a folder
    /// that somehow got lost) sorts last, which is where both the tree and the
    /// picker put it.
    pub fn folder_rank(&self, folder: Option<&str>) -> usize {
        folder
            .and_then(|folder| self.folder_index(folder))
            .unwrap_or(usize::MAX)
    }

    /// The macro `connection` runs once it is up. A connection saved after
    /// ADR-F35 names the folder as well, which is the only way to tell 「ルータ
    /// 用/ls」 from 「A-PC用/ls」; an older one only has the name.
    ///
    /// `macro_folder` is left out of the file when it is `None`, so 「saved
    /// before folders existed」 and 「explicitly 未分類」 look the same on disk.
    /// The unfiled macro is therefore tried first: a connection the user made
    /// against 「(未分類)/reload」 must not end up running 「ルータ用/reload」.
    /// Only when no unfiled macro carries the name is it read as the older
    /// form, which gets the first macro carrying it.
    pub fn macro_for_connection(&self, connection: &SavedConnection) -> Option<&TerminalMacro> {
        let name = connection.macro_name.as_deref()?;
        match connection.macro_folder.as_deref() {
            Some(folder) => self.macro_named(Some(folder), name),
            None => self.macro_named(None, name).or_else(|| {
                self.macros
                    .iter()
                    .find(|terminal_macro| terminal_macro.name == name)
            }),
        }
    }

    /// Registers a [`MacroFolder`] for every folder name a macro or a
    /// connection points at that has none, keeping the order they are first
    /// mentioned in. This is what makes a file written before ADR-F35 open with
    /// its folders already in the tree; nothing is written back until the user
    /// saves something.
    pub fn ensure_macro_folders(&mut self) {
        let mut missing: Vec<&str> = Vec::new();
        let referenced = self
            .macros
            .iter()
            .filter_map(|terminal_macro| terminal_macro.folder.as_deref())
            .chain(
                self.connections
                    .iter()
                    .filter_map(|connection| connection.macro_folder.as_deref()),
            );
        for folder in referenced {
            if !self.has_folder(folder) && !missing.contains(&folder) {
                missing.push(folder);
            }
        }
        let missing = missing
            .into_iter()
            .map(MacroFolder::new)
            .collect::<Vec<_>>();
        self.macro_folders.extend(missing);
    }
}

/// Parses the sessions file. Never fails: an unusable file is logged and
/// treated as empty, so a typo cannot make the macro UI disappear.
pub fn parse_sessions_file(contents: &str) -> TerminalSessionsFile {
    if contents.trim().is_empty() {
        return TerminalSessionsFile::default();
    }
    match settings_json::parse_json_with_comments::<TerminalSessionsFile>(contents) {
        Ok(file) => file,
        Err(error) => {
            log::warn!(
                "terminal_sessions.json could not be parsed ({error:#}); \
                 continuing with no saved macros or connections"
            );
            TerminalSessionsFile::default()
        }
    }
}

pub fn serialize_sessions_file(file: &TerminalSessionsFile) -> Result<String> {
    let mut text = serde_json::to_string_pretty(file)?;
    text.push('\n');
    Ok(text)
}

/// In-memory view of `terminal_sessions.json`, kept in step with the file on
/// disk. Registered as a global so every window shares one copy.
pub struct TerminalSessionStore {
    file: TerminalSessionsFile,
    /// Whether the first read of the file has landed. Until it has, `file` is
    /// empty for want of data rather than because the user has nothing saved,
    /// and writing it back would truncate the real file.
    loaded: bool,
    fs: Arc<dyn Fs>,
    path: PathBuf,
    _watch: Task<()>,
}

struct TerminalSessionStoreGlobal(Entity<TerminalSessionStore>);

impl Global for TerminalSessionStoreGlobal {}

impl TerminalSessionStore {
    /// Returns the shared store, creating (and starting to watch) it on first
    /// use. Lazy because the terminal crate is initialized before an `Fs` is
    /// available.
    pub fn global(fs: Arc<dyn Fs>, cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<TerminalSessionStoreGlobal>() {
            return global.0.clone();
        }
        let path = paths::terminal_sessions_file().clone();
        let store = cx.new(|cx| Self::new(fs, path, cx));
        cx.set_global(TerminalSessionStoreGlobal(store.clone()));
        store
    }

    pub fn new(fs: Arc<dyn Fs>, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let (mut contents, watcher) =
            settings::watch_config_file(cx.background_executor(), fs.clone(), path.clone());
        let watch = cx.spawn(async move |this, cx| {
            // `watch_config_file` owns the file watch only for as long as its
            // task is alive, so this task has to keep it alive.
            let _watcher = watcher;
            while let Some(contents) = contents.next().await {
                let mut file = parse_sessions_file(&contents);
                // A file written before ADR-F35 has no `macro_folders`, but the
                // UI is built on that list, so the folders its macros mention
                // are put back before anything reads it.
                file.ensure_macro_folders();
                if this
                    .update(cx, |this, cx| {
                        // The very first read always notifies, even when it
                        // matches the empty default: observers render
                        // 「読み込み中…」 until then and have to be told that the
                        // (possibly empty) real contents are now in hand.
                        if !this.loaded || this.file != file {
                            this.loaded = true;
                            this.file = file;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            file: TerminalSessionsFile::default(),
            loaded: false,
            fs,
            path,
            _watch: watch,
        }
    }

    pub fn file(&self) -> &TerminalSessionsFile {
        &self.file
    }

    /// Whether the file has been read at least once. UI that writes the whole
    /// file back must not do so before this is true.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn macros(&self) -> &[TerminalMacro] {
        &self.file.macros
    }

    pub fn connections(&self) -> &[SavedConnection] {
        &self.file.connections
    }

    pub fn macro_folders(&self) -> &[MacroFolder] {
        &self.file.macro_folders
    }

    pub fn macro_named(&self, folder: Option<&str>, name: &str) -> Option<&TerminalMacro> {
        self.file.macro_named(folder, name)
    }

    pub fn macro_for_connection(&self, connection: &SavedConnection) -> Option<&TerminalMacro> {
        self.file.macro_for_connection(connection)
    }

    /// Replaces the whole file and writes it back. The in-memory copy is
    /// updated right away so the UI does not have to wait for the watch to
    /// round-trip.
    pub fn write(
        &mut self,
        file: TerminalSessionsFile,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let mut file = file;
        // Every write is also where the folders a pre-ADR-F35 file only implied
        // become part of it, and where a folder a hand-edited file forgot to
        // declare is put back.
        file.ensure_macro_folders();
        self.file = file;
        self.loaded = true;
        cx.notify();
        let text = match serialize_sessions_file(&self.file) {
            Ok(text) => text,
            Err(error) => return Task::ready(Err(error)),
        };
        let fs = self.fs.clone();
        let path = self.path.clone();
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                fs.create_dir(parent)
                    .await
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let target = fs
                .canonicalize(&path)
                .await
                .unwrap_or_else(|_| path.clone());
            fs.atomic_write(target, text)
                .await
                .context(i18n::t!("Couldn't write terminal_sessions.json"))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacroProgress {
    pub step_index: usize,
    pub step_count: usize,
    /// 1-based repetition currently running.
    pub iteration: usize,
    /// Total repetitions, or `None` while repeating until cancelled.
    pub iteration_count: Option<usize>,
    /// Where a [`MacroStep::Repeat`] inside the current step stands, if one is
    /// running. Its steps are deliberately left out of `step_count`: the outer
    /// position is what says how far the macro as a whole has got.
    pub nested: Option<NestedProgress>,
    /// Which way the innermost 分岐 / タイムアウト分岐 currently running went.
    /// An annotation rather than a position: it says what the macro decided,
    /// and the step counters keep saying where it is.
    pub branch: Option<MacroBranch>,
}

/// Position inside a running [`MacroStep::Repeat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedProgress {
    pub iteration: usize,
    pub iteration_count: Option<usize>,
    pub step_index: usize,
    pub step_count: usize,
    /// [`MacroStep::kind_label`] of the step being run, so the status strip can
    /// name it without carrying an owned string through a `Copy` struct.
    pub kind_label: &'static str,
}

/// Runs `terminal_macro` against `terminal`, step by step, `repeat` times.
///
/// Dropping the returned task cancels the run between steps and mid-wait.
/// `on_progress` is called before each step so the caller can render
/// 「実行中 (3/7)」.
pub fn run_macro(
    terminal: Entity<Terminal>,
    terminal_macro: TerminalMacro,
    on_progress: impl Fn(MacroProgress, &mut App) + 'static,
    cx: &mut App,
) -> Task<Result<()>> {
    if let Some((label, step)) = first_unknown_step(&terminal_macro.steps, MacroStepList::Main, "")
    {
        let message = match step.unknown_kind() {
            Some(kind) => i18n::t!(
                "Step {label} is of a kind this version can't run (“{kind}”). The macro was not run."
            ),
            None => {
                i18n::t!("Step {label} is of a kind this version can't run. The macro was not run.")
            }
        };
        return Task::ready(Err(anyhow!(message)));
    }
    let step_count = terminal_macro.steps.len();
    let iteration_count = terminal_macro.iteration_count();
    let interval = Duration::from_millis(terminal_macro.repeat_interval_ms);
    cx.spawn(async move |cx| {
        let mut runner = MacroRunner {
            terminal,
            on_progress: Box::new(on_progress),
            cx: cx.clone(),
            baseline: None,
            send_baseline: None,
            variables: HashMap::new(),
            progress: MacroProgress {
                step_index: 0,
                step_count,
                iteration: 1,
                iteration_count,
                nested: None,
                branch: None,
            },
            path: Vec::new(),
        };

        let mut iteration = 0usize;
        loop {
            iteration += 1;
            if iteration > 1 {
                if interval.is_zero() {
                    // This runs on the foreground thread, so a repeat whose
                    // steps never await (nothing but 送信) would otherwise
                    // starve the UI for as long as it runs.
                    yield_now().await;
                } else {
                    let timer = runner.cx.background_executor().timer(interval);
                    timer.await;
                }
            }
            runner.progress.iteration = iteration;
            runner.run_steps(&terminal_macro.steps).await?;

            match iteration_count {
                Some(count) if iteration >= count => break,
                // A macro with nothing to do would spin forever, and cancelling
                // it means noticing a strip that never gets a chance to render.
                None if step_count == 0 => break,
                _ => {}
            }
        }

        Ok(())
    })
}

/// The first [`MacroStep::Unknown`] in `steps` or under them, in the order
/// they are written, with its position spelled the way
/// [`MacroRunner::step_label`] spells it.
fn first_unknown_step<'a>(
    steps: &'a [MacroStep],
    list: MacroStepList,
    parent: &str,
) -> Option<(String, &'a MacroStep)> {
    for (index, step) in steps.iter().enumerate() {
        let position = format!("{}{}", list.prefix(), index + 1);
        let label = if parent.is_empty() {
            position
        } else {
            format!("{parent}-{position}")
        };
        let found = match step {
            MacroStep::Unknown(_) => Some((label, step)),
            MacroStep::Repeat { steps, .. } => {
                first_unknown_step(steps, MacroStepList::Main, &label)
            }
            MacroStep::If {
                then_steps,
                else_steps,
                ..
            } => first_unknown_step(then_steps, MacroStepList::Main, &label)
                .or_else(|| first_unknown_step(else_steps, MacroStepList::Else, &label)),
            MacroStep::WaitFor {
                on_timeout: Some(steps),
                ..
            } => first_unknown_step(steps, MacroStepList::Timeout, &label),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// One in-flight macro run. Holds the state that every step level shares: the
/// terminal, the baseline a wait measures new output against, and where the
/// run currently is.
struct MacroRunner {
    terminal: Entity<Terminal>,
    on_progress: Box<dyn Fn(MacroProgress, &mut App)>,
    cx: AsyncApp,
    /// `None` means "no 送信 step has run yet", in which case a wait may match
    /// text that was already on screen (the login banner of a terminal the
    /// macro was launched against). Once a step has sent something, the waits
    /// only look at output that arrived afterwards, so a stale prompt cannot
    /// satisfy them instantly.
    baseline: Option<Vec<String>>,
    /// The grid as the last 送信 left it, which is what
    /// [`MatchScope::SinceLastSend`] measures against. Kept apart from
    /// `baseline`, because a satisfied wait moves that one forward — a 分岐
    /// placed after a wait would otherwise be looking at an empty window and
    /// could never match the answer it was put there to inspect.
    send_baseline: Option<Vec<String>>,
    /// Values captured by [`MacroStep::WaitFor`] and [`MacroStep::If`] or set
    /// by [`MacroStep::SetVariable`], expanded into later steps as `${name}`.
    /// They live for the whole run, repetitions included, so a value read on
    /// the first pass is still there on the second.
    variables: HashMap<String, String>,
    progress: MacroProgress,
    /// Position of the running step, outermost first, so a failure inside a
    /// nested repeat can name 「ステップ 3-2」. The list each level came from is
    /// carried alongside its index, so [`Self::step_label`] can tell the two
    /// sides of a 分岐 (and a wait's タイムアウト時 list) apart.
    path: Vec<(MacroStepList, usize)>,
}

impl MacroRunner {
    fn report(&self) {
        let progress = self.progress;
        self.cx.update(|cx| (self.on_progress)(progress, cx));
    }

    /// 「3」 for a top-level step, 「3-2」 for the second step of the repeat that
    /// is the third step of the macro, 「3-e2」 for the second step of the
    /// 「それ以外」 side of the 分岐 that is the third step of the macro. Matches
    /// the spelling of `StepPath::label` in `terminal_view`'s macro editor, so
    /// an error message and the row the editor highlights name the same step.
    fn step_label(&self) -> String {
        self.path
            .iter()
            .map(|(list, index)| format!("{}{}", list.prefix(), index + 1))
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Runs the outermost step list, which is the one the progress strip counts.
    async fn run_steps(&mut self, steps: &[MacroStep]) -> Result<()> {
        for (index, step) in steps.iter().enumerate() {
            self.progress.step_index = index;
            self.progress.nested = None;
            self.path.push((MacroStepList::Main, index));
            self.report();
            let result = self.run_step(step).await;
            self.path.pop();
            result?;
        }
        Ok(())
    }

    /// Runs a nested [`MacroStep::Repeat`]. Boxed because a repeat may hold
    /// another one, which would otherwise make this future infinitely sized.
    fn run_repeat<'a>(
        &'a mut self,
        count: u32,
        steps: &'a [MacroStep],
    ) -> LocalBoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let iteration_count = (count > 0).then_some(count as usize);
            let step_count = steps.len();
            // A repeat two levels down overwrites the field while it runs, so
            // the level above has to put its own position back afterwards.
            let enclosing = self.progress.nested;
            let mut iteration = 0usize;
            loop {
                iteration += 1;
                if iteration > 1 {
                    // Nothing separates the repetitions, so a body that never
                    // awaits would starve the foreground thread.
                    yield_now().await;
                }

                for (index, step) in steps.iter().enumerate() {
                    self.progress.nested = Some(NestedProgress {
                        iteration,
                        iteration_count,
                        step_index: index,
                        step_count,
                        kind_label: step.kind_label(),
                    });
                    self.path.push((MacroStepList::Main, index));
                    self.report();
                    let result = self.run_step(step).await;
                    self.path.pop();
                    result?;
                }

                match iteration_count {
                    Some(count) if iteration >= count => break,
                    None if step_count == 0 => break,
                    _ => {}
                }
            }
            self.progress.nested = enclosing;
            Ok(())
        })
    }

    /// Runs the body of a branch — either side of a [`MacroStep::If`], or the
    /// 「タイムアウト時」 list of a [`MacroStep::WaitFor`]. Boxed for the same
    /// reason as [`Self::run_repeat`]: the body may hold another branch.
    ///
    /// The enclosing step's counters are left alone, because the branch is part
    /// of that step rather than a step of its own; only `branch` is set, so the
    /// strip can say which way the run went.
    fn run_branch<'a>(
        &'a mut self,
        branch: MacroBranch,
        steps: &'a [MacroStep],
    ) -> LocalBoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let enclosing = self.progress.branch;
            self.progress.branch = Some(branch);
            let list = branch.step_list();
            for (index, step) in steps.iter().enumerate() {
                self.path.push((list, index));
                self.report();
                let result = self.run_step(step).await;
                self.path.pop();
                result?;
            }
            self.progress.branch = enclosing;
            Ok(())
        })
    }

    /// Substitutes `${name}` in a step's text against the variables captured so
    /// far.
    fn expand(&self, text: &str) -> String {
        expand_variables(text, &self.variables)
    }

    /// Files what a [`MacroStep::WaitFor`] or a [`MacroStep::If`] matched.
    /// Named regex groups always land under their own names; `capture`
    /// additionally names the match as a whole (the matched line, for a
    /// substring pattern). A name already in use is overwritten, so a step
    /// that does not match leaves whatever an earlier step captured in place.
    fn store_captures(&mut self, capture: Option<&String>, matched: &PatternMatch) {
        for (name, value) in &matched.named_groups {
            self.variables.insert(name.clone(), value.clone());
        }
        if let Some(capture) = capture {
            self.variables
                .insert(capture.clone(), matched.matched.clone());
        }
    }

    /// The lines a [`MacroStep::If`] decides on: everything the grid holds,
    /// only what arrived after the last 送信, or a variable's value.
    fn lines_in_scope(&mut self, scope: &MatchScope) -> Vec<String> {
        let baseline = match scope {
            MatchScope::Variable(name) => {
                return self.variables.get(name).cloned().into_iter().collect();
            }
            MatchScope::Screen => None,
            MatchScope::SinceLastSend => self.send_baseline.as_deref(),
        };
        let current = scan_lines(&self.terminal, &mut self.cx);
        match baseline {
            // Before the first 送信 there is no window to narrow to, so the
            // branch decides on whatever the terminal was already showing —
            // the same rule a wait follows.
            None => current,
            Some(baseline) => new_output_since(baseline, &current),
        }
    }

    async fn run_step(&mut self, step: &MacroStep) -> Result<()> {
        match step {
            MacroStep::Send { text, newline } => {
                let before = scan_lines(&self.terminal, &mut self.cx);
                self.send_baseline = Some(before.clone());
                self.baseline = Some(before);
                let mut bytes = self.expand(text).into_bytes();
                if *newline {
                    // TeraTerm sends CR for Enter; LF puts PowerShell into its
                    // continuation mode.
                    bytes.push(b'\r');
                }
                self.terminal.update(&mut self.cx, |terminal, _| {
                    terminal.write_init_command(bytes)
                });
            }
            MacroStep::Sleep { ms } => {
                let timer = self
                    .cx
                    .background_executor()
                    .timer(Duration::from_millis(*ms));
                timer.await;
            }
            MacroStep::Clear {} => {
                // The screen a wait looks at is gone, so the next wait has to
                // start from whatever the cleared grid holds.
                self.terminal
                    .update(&mut self.cx, |terminal, _| terminal.clear());
                self.baseline = None;
                self.send_baseline = None;
            }
            MacroStep::Repeat { count, steps } => {
                self.run_repeat(*count, steps).await?;
            }
            MacroStep::WaitFor {
                pattern,
                regex,
                timeout_ms,
                capture,
                on_timeout,
            } => {
                let (matcher, display_pattern) =
                    compile_step_pattern(pattern, *regex, &self.variables, &self.step_label())?;
                match wait_for_pattern(
                    &self.terminal,
                    &matcher,
                    self.baseline.as_deref(),
                    Duration::from_millis(*timeout_ms),
                    &mut self.cx,
                )
                .await
                {
                    Ok(hit) => {
                        self.store_captures(capture.as_ref(), &hit.matched);
                        self.baseline = Some(hit.lines);
                    }
                    Err(WaitFailure::TimedOut) => match on_timeout {
                        // The wait failed, so the baseline still marks the last
                        // 送信: a later wait keeps measuring from there rather
                        // than from the output the timeout gave up on.
                        Some(steps) => self.run_branch(MacroBranch::Timeout, steps).await?,
                        None => bail!(i18n::t!(
                            "Step {}: “{display_pattern}” did not appear within {} s",
                            self.step_label(),
                            *timeout_ms as f64 / 1000.0
                        )),
                    },
                    Err(WaitFailure::TerminalGone) => bail!(i18n::t!(
                        "Step {}: the terminal exited while waiting for “{display_pattern}”",
                        self.step_label()
                    )),
                }
            }
            MacroStep::If {
                pattern,
                regex,
                scope,
                capture,
                then_steps,
                else_steps,
            } => {
                let (matcher, _) =
                    compile_step_pattern(pattern, *regex, &self.variables, &self.step_label())?;
                let lines = self.lines_in_scope(scope);
                if let Some(matched) = matcher.find(&lines) {
                    self.store_captures(capture.as_ref(), &matched);
                    self.run_branch(MacroBranch::Then, then_steps).await?;
                } else {
                    self.run_branch(MacroBranch::Else, else_steps).await?;
                }
            }
            MacroStep::SetVariable { name, value } => {
                let value = self.expand(value);
                self.variables.insert(name.clone(), value);
            }
            // `run_macro` refuses a macro holding one before anything runs.
            MacroStep::Unknown(_) => bail!(i18n::t!(
                "Step {}: this version can't run this kind of step",
                self.step_label()
            )),
        }
        Ok(())
    }
}

/// Builds the matcher of a wait's or a branch's pattern against the variables
/// set so far, and the pattern as an error message should quote it.
///
/// In a regular expression a value spliced in has to stand for itself: what a
/// previous step read off the terminal is text to find, not a sub-pattern the
/// user wrote. The expanded pattern can still fail to compile where the one the
/// editor checked did not — `(${p}+)` with `p` set to the empty string is `(+)` —
/// and the run then stops on that step rather than guessing what was meant.
fn compile_step_pattern(
    pattern: &str,
    regex: bool,
    variables: &HashMap<String, String>,
    step_label: &str,
) -> Result<(PatternMatcher, String)> {
    let display_pattern = expand_variables(pattern, variables);
    let compiled_pattern = if regex {
        expand_variables_into_regex(pattern, variables)
    } else {
        display_pattern.clone()
    };
    // `error` quotes the compiled pattern, where the values are escaped (a
    // captured `10.0.0.1` shows up as `10\.0\.0\.1`), so the pattern the
    // user wrote has to be named alongside it.
    let matcher = PatternMatcher::new(&compiled_pattern, regex).map_err(|error| {
        anyhow!(i18n::t!(
            "Step {step_label}: the regular expression “{display_pattern}” is invalid ({error})"
        ))
    })?;
    Ok((matcher, display_pattern))
}

/// Replaces every `${name}` in `text` with the matching variable.
///
/// An unknown name is left exactly as written instead of turning into an empty
/// string, so a typo shows up in the terminal rather than sending a truncated
/// command to a switch. `$${` is the escape for a literal `${`.
pub fn expand_variables(text: &str, variables: &HashMap<String, String>) -> String {
    expand_variables_with(text, variables, false)
}

/// [`expand_variables`] for a pattern that is about to be compiled as a regular
/// expression.
///
/// The values go in as the literal text they are: a captured `10.0.0.1` has to
/// match that address and not any character where its dots are, and a captured
/// `Invalid input (config)` has to compile at all. What the user wrote around
/// the references stays a regular expression.
pub fn expand_variables_into_regex(text: &str, variables: &HashMap<String, String>) -> String {
    expand_variables_with(text, variables, true)
}

fn expand_variables_with(
    text: &str,
    variables: &HashMap<String, String>,
    escape_values: bool,
) -> String {
    let mut expanded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        expanded.push_str(&rest[..dollar]);
        let after = &rest[dollar..];
        if let Some(escaped) = after.strip_prefix("$${") {
            // In a regular expression a bare `${` does not compile, so the
            // escape has to come out as the two characters it stands for.
            expanded.push_str(if escape_values { r"\$\{" } else { "${" });
            rest = escaped;
            continue;
        }
        let Some(opened) = after.strip_prefix("${") else {
            expanded.push('$');
            rest = &after[1..];
            continue;
        };
        let Some(closed) = opened.find('}') else {
            expanded.push_str(after);
            return expanded;
        };
        let name = &opened[..closed];
        let written = &after[..closed + 3];
        match variables.get(name) {
            Some(value) if escape_values => expanded.push_str(&regex::escape(value)),
            Some(value) => expanded.push_str(value),
            // Left as written means as the literal text in a regex too: a bare
            // `${name}` does not compile, so a run that reaches the pattern
            // before anything set the name (a capture on a branch side not
            // taken) would stop on 「正規表現が不正です」 for a pattern the
            // editor accepted, where a substring pattern just finds nothing.
            None if escape_values => expanded.push_str(&regex::escape(written)),
            None => expanded.push_str(written),
        }
        rest = &opened[closed + 1..];
    }
    expanded.push_str(rest);
    expanded
}

/// The names `text` refers to as `${name}`, in order of first appearance, with
/// the `$${` escape skipped the same way [`expand_variables`] skips it.
pub fn referenced_variables(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        let after = &rest[dollar..];
        if let Some(escaped) = after.strip_prefix("$${") {
            rest = escaped;
            continue;
        }
        let Some(opened) = after.strip_prefix("${") else {
            rest = &after[1..];
            continue;
        };
        let Some(closed) = opened.find('}') else {
            break;
        };
        let name = &opened[..closed];
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
        rest = &opened[closed + 1..];
    }
    names
}

/// Compiles `pattern` the way a run would, with every `${name}` standing for
/// a literal. What the variables will hold is unknown until the macro runs, so
/// this is what the editor can check at save time: that the regular expression
/// the user wrote around the references is valid.
pub fn check_pattern_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let placeholders = referenced_variables(pattern)
        .into_iter()
        .map(|name| (name, "x".to_string()))
        .collect();
    regex::Regex::new(&expand_variables_into_regex(pattern, &placeholders))
}

/// The names of the `${name}` references in a regular expression that a
/// repetition (`*`, `+`, `?`, `{n}`) follows directly. The repetition applies
/// to the last character of the value only, and when the value is empty to
/// whatever precedes the reference, or to nothing, which does not compile.
/// [`check_pattern_regex`] cannot see that, because it stands a one-character
/// literal in for every reference.
pub fn references_followed_by_repetition(pattern: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = pattern;
    while let Some(dollar) = rest.find('$') {
        let after = &rest[dollar..];
        if let Some(escaped) = after.strip_prefix("$${") {
            rest = escaped;
            continue;
        }
        let Some(opened) = after.strip_prefix("${") else {
            rest = &after[1..];
            continue;
        };
        let Some(closed) = opened.find('}') else {
            break;
        };
        let name = &opened[..closed];
        rest = &opened[closed + 1..];
        let mut following = rest.chars();
        let repeated = match following.next() {
            Some('*' | '+' | '?') => true,
            Some('{') => following.next().is_some_and(|next| next.is_ascii_digit()),
            _ => false,
        };
        if repeated && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn scan_lines(terminal: &Entity<Terminal>, cx: &mut AsyncApp) -> Vec<String> {
    terminal.read_with(cx, |terminal, _| {
        terminal.last_n_non_empty_lines(WAIT_SCAN_LINES)
    })
}

/// Why a [`MacroStep::WaitFor`] gave up. Kept separate from the message so the
/// caller can name the step and the pattern in one sentence.
enum WaitFailure {
    TimedOut,
    TerminalGone,
}

/// A satisfied [`MacroStep::WaitFor`].
struct WaitHit {
    /// The tail at the moment of the match, to be used as the next baseline.
    lines: Vec<String>,
    matched: PatternMatch,
}

/// Waits until `matcher` hits the output that arrived after `baseline`.
async fn wait_for_pattern(
    terminal: &Entity<Terminal>,
    matcher: &PatternMatcher,
    baseline: Option<&[String]>,
    timeout: Duration,
    cx: &mut AsyncApp,
) -> Result<WaitHit, WaitFailure> {
    let (wake_tx, wake_rx) = async_channel::unbounded::<()>();
    // Registered before the first check so output produced while we look is
    // still guaranteed to wake us.
    let _subscription = cx.update(|cx| {
        cx.subscribe(terminal, move |_, event: &Event, _| {
            if matches!(event, Event::Wakeup) {
                match wake_tx.try_send(()) {
                    Ok(()) => {}
                    Err(async_channel::TrySendError::Full(())) => {}
                    Err(async_channel::TrySendError::Closed(())) => {}
                }
            }
        })
    });

    // `async_channel::Receiver` is not `Unpin`, so it is pinned once here
    // rather than boxing a fresh future on every wakeup.
    let mut wake_rx = Box::pin(wake_rx);
    let mut timeout_timer = cx.background_executor().timer(timeout);
    loop {
        let current = scan_lines(terminal, cx);
        let scanned = match baseline {
            Some(baseline) => new_output_since(baseline, &current),
            None => current.clone(),
        };
        if let Some(matched) = matcher.find(&scanned) {
            return Ok(WaitHit {
                lines: current,
                matched,
            });
        }

        let mut next_wake = wake_rx.next();
        match futures::future::select(&mut next_wake, &mut timeout_timer).await {
            futures::future::Either::Left((Some(()), _)) => continue,
            futures::future::Either::Left((None, _)) => {
                return Err(WaitFailure::TerminalGone);
            }
            futures::future::Either::Right(((), _)) => return Err(WaitFailure::TimedOut),
        }
    }
}

pub enum PatternMatcher {
    Substring(String),
    Regex(regex::Regex),
}

/// What a [`PatternMatcher`] found, so a step can file it under a variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternMatch {
    /// The whole line the pattern hit.
    pub line: String,
    /// The text the pattern itself covers: the regex match, or — for a
    /// substring pattern, where matching the search text back would be
    /// useless — the whole line.
    pub matched: String,
    /// Named groups (`(?P<name>…)`) of a regex pattern, in the order the
    /// pattern declares them.
    pub named_groups: Vec<(String, String)>,
}

impl PatternMatcher {
    pub fn new(pattern: &str, regex: bool) -> Result<Self> {
        if regex {
            Ok(PatternMatcher::Regex(regex::Regex::new(pattern)?))
        } else {
            Ok(PatternMatcher::Substring(pattern.to_string()))
        }
    }

    /// The first line of `lines` the pattern hits, with whatever it captured
    /// there. First rather than last, so a paged banner that repeats a prompt
    /// yields the answer that provoked the wait.
    pub fn find(&self, lines: &[String]) -> Option<PatternMatch> {
        lines.iter().find_map(|line| match self {
            PatternMatcher::Substring(pattern) => {
                line.contains(pattern.as_str()).then(|| PatternMatch {
                    line: line.clone(),
                    matched: line.clone(),
                    named_groups: Vec::new(),
                })
            }
            PatternMatcher::Regex(regex) => regex.captures(line).map(|captures| PatternMatch {
                line: line.clone(),
                matched: captures
                    .get(0)
                    .map(|whole| whole.as_str().to_string())
                    .unwrap_or_default(),
                named_groups: regex
                    .capture_names()
                    .flatten()
                    .filter_map(|name| {
                        let value = captures.name(name)?;
                        Some((name.to_string(), value.as_str().to_string()))
                    })
                    .collect(),
            }),
        })
    }

    pub fn matches(&self, lines: &[String]) -> bool {
        self.find(lines).is_some()
    }
}

/// The part of `current` that was not already in `baseline`.
///
/// Both are trailing windows of the same scrollback, so `current` is `baseline`
/// shifted left by however many lines arrived since. The last baseline line is
/// matched by prefix rather than equality, because a line that had not been
/// terminated yet (`Password: `) can still grow; when it does, it counts as new
/// output. If no alignment is found at all (the screen was cleared, or an
/// application switched to the alternate screen) everything is new.
pub fn new_output_since(baseline: &[String], current: &[String]) -> Vec<String> {
    for skipped in 0..=baseline.len() {
        let tail = &baseline[skipped..];
        if tail.len() > current.len() {
            continue;
        }
        let Some((last_baseline, leading)) = tail.split_last() else {
            return current.to_vec();
        };
        if leading != &current[..leading.len()] {
            continue;
        }
        let aligned = &current[leading.len()];
        if aligned == last_baseline {
            return current[tail.len()..].to_vec();
        }
        if aligned.starts_with(last_baseline.as_str()) {
            return current[leading.len()..].to_vec();
        }
    }
    current.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    /// A `terminal_sessions.json` written before 分岐 could take a 取り込む変数名:
    /// the real file shape (`connections` + `macros`) with one of every step
    /// kind the format had then, the `if` deliberately carrying no `capture`.
    const LEGACY_SESSIONS_FILE: &str = r#"{
        "connections": [
            {
                "name": "本社ルーター",
                "protocol": "ssh",
                "host": "10.0.0.1",
                "user": "admin",
                "macro": "点検"
            }
        ],
        "macros": [
            {
                "name": "点検",
                "steps": [
                    { "type": "send", "text": "show version" },
                    {
                        "type": "wait_for",
                        "pattern": "router#",
                        "timeout_ms": 5000,
                        "capture": "prompt"
                    },
                    { "type": "sleep", "ms": 500 },
                    { "type": "clear" },
                    {
                        "type": "repeat",
                        "count": 2,
                        "steps": [{ "type": "send", "text": "show ip route" }]
                    },
                    {
                        "type": "if",
                        "pattern": "Invalid input",
                        "scope": "screen",
                        "then": [{ "type": "send", "text": "enable" }],
                        "else": [{ "type": "send", "text": "exit" }]
                    }
                ]
            }
        ]
    }"#;

    /// [`LEGACY_SESSIONS_FILE`] the way this build writes it back. The serde
    /// defaults it left out (`newline`, `regex`) are spelled in, and — the
    /// point of the fixture — the `if` step still has no `capture` key, so a
    /// user who opens one macro in the editor does not get every branch in the
    /// file rewritten.
    const LEGACY_SESSIONS_FILE_REWRITTEN: &str = r#"{
  "connections": [
    {
      "name": "本社ルーター",
      "protocol": "ssh",
      "host": "10.0.0.1",
      "user": "admin",
      "macro": "点検"
    }
  ],
  "macros": [
    {
      "name": "点検",
      "steps": [
        {
          "type": "send",
          "text": "show version",
          "newline": true
        },
        {
          "type": "wait_for",
          "pattern": "router#",
          "regex": false,
          "timeout_ms": 5000,
          "capture": "prompt"
        },
        {
          "type": "sleep",
          "ms": 500
        },
        {
          "type": "clear"
        },
        {
          "type": "repeat",
          "count": 2,
          "steps": [
            {
              "type": "send",
              "text": "show ip route",
              "newline": true
            }
          ]
        },
        {
          "type": "if",
          "pattern": "Invalid input",
          "regex": false,
          "scope": "screen",
          "then": [
            {
              "type": "send",
              "text": "enable",
              "newline": true
            }
          ],
          "else": [
            {
              "type": "send",
              "text": "exit",
              "newline": true
            }
          ]
        }
      ]
    }
  ]
}
"#;

    #[test]
    fn round_trips_the_sessions_file() {
        let file = TerminalSessionsFile {
            connections: vec![SavedConnection {
                name: "本社ルーター".to_string(),
                folder: Some("拠点/本社".to_string()),
                protocol: ConnectionProtocol::Telnet,
                host: "192.168.1.1".to_string(),
                port: None,
                user: None,
                macro_name: Some("ログイン".to_string()),
                macro_folder: Some("拠点/本社".to_string()),
                log: Some(true),
                remember_password: true,
                transfer_protocol: Some("ftp".to_string()),
                transfer_port: Some(2121),
            }],
            macro_folders: vec![
                MacroFolder::new("拠点/本社"),
                MacroFolder {
                    name: "拠点/大阪".to_string(),
                    collapsed: true,
                },
            ],
            macros: vec![TerminalMacro {
                name: "ログイン".to_string(),
                folder: Some("拠点/本社".to_string()),
                tags: vec!["ルーター".to_string()],
                description: "特権モードまで進む".to_string(),
                repeat: 3,
                repeat_interval_ms: 500,
                steps: vec![
                    MacroStep::WaitFor {
                        pattern: "Password:".to_string(),
                        regex: false,
                        timeout_ms: 5_000,
                        capture: None,
                        on_timeout: None,
                    },
                    MacroStep::Send {
                        text: "secret".to_string(),
                        newline: true,
                    },
                    MacroStep::Sleep { ms: 500 },
                ],
            }],
        };

        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(text.contains(r#""collapsed": true"#), "{text}");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn the_same_name_in_two_folders_is_two_macros() {
        let file = parse_sessions_file(
            r#"{
                "macro_folders": [{ "name": "A-PC用" }, { "name": "B-PC用" }],
                "macros": [
                    { "name": "ls", "folder": "A-PC用", "description": "A", "steps": [] },
                    { "name": "ls", "folder": "B-PC用", "description": "B", "steps": [] },
                    { "name": "ls", "description": "未分類", "steps": [] }
                ]
            }"#,
        );

        assert_eq!(
            file.macro_named(Some("A-PC用"), "ls")
                .map(|found| found.description.as_str()),
            Some("A")
        );
        assert_eq!(
            file.macro_named(Some("B-PC用"), "ls")
                .map(|found| found.description.as_str()),
            Some("B")
        );
        assert_eq!(
            file.macro_named(None, "ls")
                .map(|found| found.description.as_str()),
            Some("未分類")
        );
        assert_eq!(file.macro_named(Some("C-PC用"), "ls"), None);
        assert_eq!(file.macros_in_folder(Some("A-PC用")).count(), 1);
        assert_eq!(file.macros_in_folder(None).count(), 1);
        assert_eq!(
            file.macros[0].key(),
            (Some("A-PC用".to_string()), "ls".to_string())
        );
    }

    #[test]
    fn a_connection_resolves_its_macro_by_folder_and_name() {
        let mut file = parse_sessions_file(
            r#"{
                "macro_folders": [{ "name": "ルータ用" }, { "name": "A-PC用" }],
                "macros": [
                    { "name": "ls", "folder": "ルータ用", "description": "router", "steps": [] },
                    { "name": "ls", "folder": "A-PC用", "description": "pc", "steps": [] }
                ]
            }"#,
        );
        let mut connection = SavedConnection {
            name: "r1".into(),
            host: "10.0.0.1".into(),
            macro_name: Some("ls".to_string()),
            macro_folder: Some("A-PC用".to_string()),
            ..SavedConnection::default()
        };
        file.connections = vec![connection.clone()];
        assert_eq!(
            file.macro_for_connection(&connection)
                .map(|found| found.description.as_str()),
            Some("pc")
        );

        // A file written before ADR-F35 has only the name, and gets the first
        // macro carrying it.
        connection.macro_folder = None;
        assert_eq!(
            file.macro_for_connection(&connection)
                .map(|found| found.description.as_str()),
            Some("router")
        );

        connection.macro_folder = Some("消えたフォルダー".to_string());
        assert_eq!(file.macro_for_connection(&connection), None);
        connection.macro_name = None;
        assert_eq!(file.macro_for_connection(&connection), None);
    }

    #[test]
    fn a_connection_without_a_folder_prefers_the_unfiled_macro() {
        let file = parse_sessions_file(
            r#"{
                "macro_folders": [{ "name": "ルータ用" }],
                "macros": [
                    { "name": "reload", "folder": "ルータ用", "description": "router", "steps": [] },
                    { "name": "reload", "description": "未分類", "steps": [] }
                ]
            }"#,
        );
        // `macro_folder` is omitted from the file when it is `None`, so this is
        // both what an explicitly 未分類 reference and a pre-ADR-F35 one look
        // like. 未分類 wins, because that is the only reading the user can have
        // asked for.
        let connection = SavedConnection {
            name: "r1".into(),
            host: "10.0.0.1".into(),
            macro_name: Some("reload".to_string()),
            macro_folder: None,
            ..SavedConnection::default()
        };
        assert_eq!(
            file.macro_for_connection(&connection)
                .map(|found| found.description.as_str()),
            Some("未分類")
        );

        // With nothing unfiled by that name it is the older form after all, and
        // still finds the foldered macro.
        let file = parse_sessions_file(
            r#"{
                "macro_folders": [{ "name": "ルータ用" }],
                "macros": [
                    { "name": "reload", "folder": "ルータ用", "description": "router", "steps": [] }
                ]
            }"#,
        );
        assert_eq!(
            file.macro_for_connection(&connection)
                .map(|found| found.description.as_str()),
            Some("router")
        );
    }

    #[test]
    fn a_legacy_file_gets_its_folders_back_in_first_seen_order() {
        let text = r#"{
            "connections": [
                { "name": "r1", "host": "10.0.0.1", "macro": "ls", "macro_folder": "接続だけ" }
            ],
            "macros": [
                { "name": "ls", "folder": "B-PC用", "steps": [] },
                { "name": "show run", "folder": "ルータ用", "steps": [] },
                { "name": "ls", "folder": "B-PC用", "steps": [] },
                { "name": "メモ", "steps": [] }
            ]
        }"#;
        let file = parse_sessions_file(text);
        // Parsing on its own changes nothing, so an untouched old file is
        // rewritten exactly as it was read.
        assert!(file.macro_folders.is_empty());
        let resaved = serialize_sessions_file(&file).expect("serializes");
        assert!(!resaved.contains("macro_folders"), "{resaved}");

        let mut repaired = file;
        repaired.ensure_macro_folders();
        assert_eq!(
            repaired
                .macro_folders
                .iter()
                .map(|folder| folder.name.as_str())
                .collect::<Vec<_>>(),
            vec!["B-PC用", "ルータ用", "接続だけ"],
            "macros are seen first, in file order, then the connections"
        );
        assert!(
            repaired
                .macro_folders
                .iter()
                .all(|folder| !folder.collapsed)
        );
        assert_eq!(repaired.folder_rank(Some("ルータ用")), 1);
        assert_eq!(repaired.folder_rank(None), usize::MAX);
        assert_eq!(repaired.folder_rank(Some("知らない")), usize::MAX);

        // Repairing an already repaired file adds nothing.
        let twice = {
            let mut twice = repaired.clone();
            twice.ensure_macro_folders();
            twice
        };
        assert_eq!(twice, repaired);
    }

    #[test]
    fn remember_password_round_trips_and_stays_out_of_the_file_when_off() {
        let mut connection = SavedConnection {
            name: "r1".into(),
            host: "10.0.0.1".into(),
            remember_password: true,
            ..SavedConnection::default()
        };
        let file = TerminalSessionsFile {
            connections: vec![connection.clone()],
            ..TerminalSessionsFile::default()
        };
        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(text.contains("remember_password"));
        // Whatever else the file grows, the password itself is never in it.
        assert!(!text.contains("password\": \""));
        assert_eq!(parse_sessions_file(&text), file);

        connection.remember_password = false;
        let file = TerminalSessionsFile {
            connections: vec![connection],
            ..TerminalSessionsFile::default()
        };
        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(!text.contains("remember_password"));
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn a_file_written_before_remembered_passwords_still_parses() {
        let file = parse_sessions_file(
            r#"{
                "connections": [
                    { "name": "r1", "host": "10.0.0.1", "user": "admin", "log": true }
                ]
            }"#,
        );
        assert!(!file.connections[0].remember_password);
        assert_eq!(file.connections[0].transfer_protocol, None);
        assert_eq!(file.connections[0].transfer_port, None);
    }

    #[test]
    fn the_transfer_protocol_is_optional_and_stays_out_of_the_file_when_unset() {
        let mut connection = SavedConnection {
            name: "r1".into(),
            host: "10.0.0.1".into(),
            ..SavedConnection::default()
        };
        let text = serialize_sessions_file(&TerminalSessionsFile {
            connections: vec![connection.clone()],
            ..TerminalSessionsFile::default()
        })
        .expect("serializes");
        assert!(!text.contains("transfer_protocol"), "{text}");
        assert!(!text.contains("transfer_port"), "{text}");

        connection.transfer_protocol = Some("scp".to_string());
        connection.transfer_port = Some(2121);
        let file = TerminalSessionsFile {
            connections: vec![connection],
            ..TerminalSessionsFile::default()
        };
        let text = serialize_sessions_file(&file).expect("serializes");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn tolerates_comments_and_missing_fields() {
        let file = parse_sessions_file(
            r#"{
                // 保存済みの接続
                "connections": [{ "name": "r1", "host": "10.0.0.1" }],
                "macros": [{
                    "name": "m1",
                    "steps": [
                        { "type": "send", "text": "show version" },
                        { "type": "wait_for", "pattern": "PROMPT" },
                        { "type": "sleep", "ms": 100 }
                    ]
                }]
            }"#,
        );

        let connection = &file.connections[0];
        assert_eq!(connection.protocol, ConnectionProtocol::Ssh);
        assert_eq!(connection.port_or_default(), 22);
        assert_eq!(
            connection.command(),
            (
                "ssh".to_string(),
                vec!["-p".into(), "22".into(), "10.0.0.1".into()]
            )
        );
        assert_eq!(
            file.macros[0].steps[0],
            MacroStep::Send {
                text: "show version".to_string(),
                newline: true
            }
        );
        assert_eq!(
            file.macros[0].steps[1],
            MacroStep::WaitFor {
                pattern: "PROMPT".to_string(),
                regex: false,
                timeout_ms: DEFAULT_WAIT_TIMEOUT_MS,
                capture: None,
                on_timeout: None,
            }
        );
    }

    #[test]
    fn a_file_written_before_folders_and_repeats_still_parses() {
        let file = parse_sessions_file(
            r#"{
                "connections": [{ "name": "r1", "host": "10.0.0.1", "macro": "m1" }],
                "macros": [{
                    "name": "m1",
                    "description": "旧形式",
                    "steps": [{ "type": "send", "text": "show version" }]
                }]
            }"#,
        );

        let terminal_macro = &file.macros[0];
        assert_eq!(terminal_macro.folder, None);
        assert!(terminal_macro.tags.is_empty());
        assert_eq!(terminal_macro.repeat, 1);
        assert_eq!(terminal_macro.iteration_count(), Some(1));
        assert_eq!(terminal_macro.repeat_interval_ms, 0);
        assert_eq!(terminal_macro.qualified_name(), "m1");
        assert_eq!(file.connections[0].folder, None);

        // The defaults must not show up in the file the next time it is saved.
        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(!text.contains("folder"), "{text}");
        assert!(!text.contains("tags"), "{text}");
        assert!(!text.contains("repeat"), "{text}");
    }

    #[test]
    fn a_clear_step_round_trips() {
        let file = parse_sessions_file(
            r#"{ "macros": [{ "name": "m", "steps": [{ "type": "clear" }] }] }"#,
        );
        assert_eq!(file.macros[0].steps, vec![MacroStep::Clear {}]);
        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(text.contains(r#""type": "clear""#), "{text}");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn a_nested_repeat_round_trips() {
        let file = parse_sessions_file(
            r#"{
                "macros": [{
                    "name": "m",
                    "steps": [
                        { "type": "send", "text": "enable" },
                        {
                            "type": "repeat",
                            "count": 3,
                            "steps": [
                                { "type": "send", "text": "show version" },
                                { "type": "sleep", "ms": 500 }
                            ]
                        },
                        { "type": "repeat", "steps": [] }
                    ]
                }]
            }"#,
        );

        assert_eq!(
            file.macros[0].steps[1],
            MacroStep::Repeat {
                count: 3,
                steps: vec![
                    MacroStep::Send {
                        text: "show version".to_string(),
                        newline: true,
                    },
                    MacroStep::Sleep { ms: 500 },
                ],
            }
        );
        // A repeat without a count is a single pass, like `TerminalMacro`.
        assert_eq!(
            file.macros[0].steps[2],
            MacroStep::Repeat {
                count: 1,
                steps: Vec::new(),
            }
        );

        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(text.contains(r#""type": "repeat""#), "{text}");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn a_wait_that_uses_none_of_the_new_fields_is_written_the_old_way() {
        let file = parse_sessions_file(
            r#"{
                "macros": [{
                    "name": "m",
                    "steps": [
                        { "type": "wait_for", "pattern": "router#", "timeout_ms": 5000 }
                    ]
                }]
            }"#,
        );
        assert_eq!(
            file.macros[0].steps,
            vec![MacroStep::WaitFor {
                pattern: "router#".to_string(),
                regex: false,
                timeout_ms: 5_000,
                capture: None,
                on_timeout: None,
            }]
        );

        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(!text.contains("capture"), "{text}");
        assert!(!text.contains("on_timeout"), "{text}");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn a_wait_with_a_capture_and_a_timeout_branch_round_trips() {
        let file = parse_sessions_file(
            r#"{
                "macros": [{
                    "name": "m",
                    "steps": [{
                        "type": "wait_for",
                        "pattern": "Version (?P<version>[0-9.]+)",
                        "regex": true,
                        "timeout_ms": 5000,
                        "capture": "banner",
                        "on_timeout": [{ "type": "send", "text": "exit" }]
                    }]
                }]
            }"#,
        );
        assert_eq!(
            file.macros[0].steps,
            vec![MacroStep::WaitFor {
                pattern: "Version (?P<version>[0-9.]+)".to_string(),
                regex: true,
                timeout_ms: 5_000,
                capture: Some("banner".to_string()),
                on_timeout: Some(vec![MacroStep::Send {
                    text: "exit".to_string(),
                    newline: true,
                }]),
            }]
        );
        assert_eq!(
            parse_sessions_file(&serialize_sessions_file(&file).expect("serializes")),
            file
        );
    }

    #[test]
    fn an_empty_timeout_branch_is_kept_apart_from_no_branch_at_all() {
        let file = parse_sessions_file(
            r#"{
                "macros": [{
                    "name": "m",
                    "steps": [
                        { "type": "wait_for", "pattern": "a", "on_timeout": [] }
                    ]
                }]
            }"#,
        );
        let MacroStep::WaitFor { on_timeout, .. } = &file.macros[0].steps[0] else {
            panic!("the step should be a wait");
        };
        assert_eq!(
            on_timeout.as_deref(),
            Some(&[][..]),
            "an empty list still means carry on to the next step, so it has to survive a save"
        );
        assert_eq!(
            parse_sessions_file(&serialize_sessions_file(&file).expect("serializes")),
            file
        );
    }

    #[test]
    fn a_branch_step_round_trips() {
        let file = parse_sessions_file(
            r#"{
                "macros": [{
                    "name": "m",
                    "steps": [
                        {
                            "type": "if",
                            "pattern": "Invalid input",
                            "scope": "screen",
                            "then": [{ "type": "send", "text": "exit" }],
                            "else": [{
                                "type": "repeat",
                                "count": 2,
                                "steps": [{ "type": "send", "text": "show ip route" }]
                            }]
                        },
                        { "type": "if", "pattern": "x" },
                        {
                            "type": "if",
                            "pattern": "host=(?P<host>\\S+)",
                            "regex": true,
                            "capture": "banner"
                        }
                    ]
                }]
            }"#,
        );
        assert_eq!(
            file.macros[0].steps[0],
            MacroStep::If {
                pattern: "Invalid input".to_string(),
                regex: false,
                scope: MatchScope::Screen,
                capture: None,
                then_steps: vec![MacroStep::Send {
                    text: "exit".to_string(),
                    newline: true,
                }],
                else_steps: vec![MacroStep::Repeat {
                    count: 2,
                    steps: vec![MacroStep::Send {
                        text: "show ip route".to_string(),
                        newline: true,
                    }],
                }],
            }
        );
        assert_eq!(
            file.macros[0].steps[1],
            MacroStep::If {
                pattern: "x".to_string(),
                regex: false,
                scope: MatchScope::SinceLastSend,
                capture: None,
                then_steps: Vec::new(),
                else_steps: Vec::new(),
            }
        );
        assert_eq!(
            file.macros[0].steps[2],
            MacroStep::If {
                pattern: r"host=(?P<host>\S+)".to_string(),
                regex: true,
                scope: MatchScope::SinceLastSend,
                capture: Some("banner".to_string()),
                then_steps: Vec::new(),
                else_steps: Vec::new(),
            }
        );

        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(text.contains(r#""type": "if""#), "{text}");
        assert!(text.contains(r#""capture": "banner""#), "{text}");
        // The default scope and the two empty branches of the second step stay
        // out of the file, so a macro the user never branched on reads back the
        // way it was written.
        assert!(!text.contains(r#""since_last_send""#), "{text}");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn a_set_variable_step_and_a_branch_on_a_variable_round_trip() {
        let file = parse_sessions_file(
            r#"{
                "macros": [{
                    "name": "m",
                    "steps": [
                        { "type": "set_variable", "name": "host", "value": "rt01" },
                        {
                            "type": "if",
                            "pattern": "^15\\.",
                            "regex": true,
                            "scope": { "variable": "ver" },
                            "then": [{ "type": "send", "text": "ping ${host}" }]
                        }
                    ]
                }]
            }"#,
        );
        assert_eq!(
            file.macros[0].steps,
            vec![
                MacroStep::SetVariable {
                    name: "host".to_string(),
                    value: "rt01".to_string(),
                },
                MacroStep::If {
                    pattern: r"^15\.".to_string(),
                    regex: true,
                    scope: MatchScope::Variable("ver".to_string()),
                    capture: None,
                    then_steps: vec![MacroStep::Send {
                        text: "ping ${host}".to_string(),
                        newline: true,
                    }],
                    else_steps: Vec::new(),
                },
            ]
        );
        assert_eq!(
            file.macros[0].steps[1].summary(),
            "Branch: if “^15\\.” is in ${ver}, run 1 step(s), otherwise 0 step(s)"
        );
        assert_eq!(
            file.macros[0].steps[0].summary(),
            "Variable: ${host} = “rt01”"
        );

        let text = serialize_sessions_file(&file).expect("serializes");
        assert!(text.contains(r#""type": "set_variable""#), "{text}");
        assert!(text.contains(r#""variable": "ver""#), "{text}");
        assert_eq!(parse_sessions_file(&text), file);
    }

    #[test]
    fn referenced_variables_are_listed_once_and_skip_the_escape() {
        assert_eq!(
            referenced_variables("ping ${host} -c ${count} ${host} $${literal} ${open"),
            vec!["host".to_string(), "count".to_string()]
        );
        assert!(referenced_variables("cost is $5").is_empty());
    }

    #[test]
    fn a_regex_pattern_with_references_is_checked_around_them() {
        // `${host}` on its own is not a valid regular expression (`$` followed
        // by a repetition), which is what used to make the editor refuse to
        // save any regex that used a variable. Held in a `String` because
        // clippy's `invalid_regex` rejects the literal at compile time.
        let raw_pattern = String::from(r"^From ${host}:");
        assert!(regex::Regex::new(&raw_pattern).is_err());
        assert!(check_pattern_regex(r"^From ${host}:").is_ok());
        assert!(check_pattern_regex(r"^From ${host}:(").is_err());
        assert!(
            check_pattern_regex(r"cost $${x}").is_ok(),
            "the escape has to stand for a literal `${{` inside a regex too"
        );
        let names = check_pattern_regex(r"Version (?<ver>[0-9.]+) on ${host}")
            .expect("compiles")
            .capture_names()
            .flatten()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["ver".to_string()]);

        let matcher = PatternMatcher::new(
            &expand_variables_into_regex(r"cost $${x}", &HashMap::default()),
            true,
        )
        .expect("compiles");
        assert!(matcher.find(&["cost ${x}".to_string()]).is_some());
    }

    #[test]
    fn a_file_from_before_the_branch_capture_is_rewritten_without_one() {
        let file = parse_sessions_file(LEGACY_SESSIONS_FILE);
        let MacroStep::If { capture, .. } = &file.macros[0].steps[5] else {
            panic!("the sixth step of the fixture is the branch");
        };
        assert_eq!(
            capture, &None,
            "a file written before the field existed must read back without it"
        );

        let written = serialize_sessions_file(&file).expect("serializes");
        assert_eq!(written, LEGACY_SESSIONS_FILE_REWRITTEN);
        // And a file this build wrote is byte for byte what the next write
        // produces, so opening a macro in the editor never churns the rest of
        // the file.
        assert_eq!(
            serialize_sessions_file(&parse_sessions_file(&written)).expect("serializes"),
            written
        );
    }

    #[test]
    fn a_branch_step_summarizes_both_sides() {
        assert_eq!(
            MacroStep::If {
                pattern: "Invalid".to_string(),
                regex: false,
                scope: MatchScope::Screen,
                capture: None,
                then_steps: vec![MacroStep::Clear {}],
                else_steps: Vec::new(),
            }
            .summary(),
            "Branch: if “Invalid” is in the whole screen, run 1 step(s), otherwise 0 step(s)"
        );
        assert_eq!(
            MacroStep::If {
                pattern: "Invalid".to_string(),
                regex: false,
                scope: MatchScope::Screen,
                capture: Some("banner".to_string()),
                then_steps: vec![MacroStep::Clear {}],
                else_steps: Vec::new(),
            }
            .summary(),
            "Branch: if “Invalid” is in the whole screen, run 1 step(s), otherwise 0 step(s), on a match capture into ${banner}"
        );
    }

    #[test]
    fn a_wait_summarizes_its_capture_and_its_timeout_branch() {
        assert_eq!(
            MacroStep::WaitFor {
                pattern: "router#".to_string(),
                regex: false,
                timeout_ms: 5_000,
                capture: Some("prompt".to_string()),
                on_timeout: Some(vec![MacroStep::Clear {}]),
            }
            .summary(),
            "Wait for “router#” (up to 5 s), capture into ${prompt}, 1 step on timeout"
        );
    }

    /// A file a later version wrote: a kind this build has never heard of, a
    /// known kind with a field added (also to 画面を消去, which has none), a
    /// 分岐 on a scope that does not exist yet, and one of them nested inside
    /// a 繰り返し.
    const FUTURE_SESSIONS_FILE: &str = r#"{
        "macros": [
            {
                "name": "future",
                "steps": [
                    { "type": "send", "text": "show version" },
                    { "type": "http_request", "url": "https://example.invalid", "retry": { "count": 3 } },
                    { "type": "send", "text": "exit", "delay_ms": 500 },
                    {
                        "type": "repeat",
                        "count": 2,
                        "steps": [
                            { "type": "sleep", "ms": 100 },
                            { "type": "beep" }
                        ]
                    },
                    { "type": "if", "pattern": "x", "scope": { "file": "log.txt" } },
                    { "type": "clear", "scrollback": false }
                ]
            },
            {
                "name": "today",
                "steps": [{ "type": "clear" }]
            }
        ]
    }"#;

    #[test]
    fn a_step_this_build_cannot_read_is_set_aside_and_written_back_as_it_was() {
        let file = parse_sessions_file(FUTURE_SESSIONS_FILE);
        assert_eq!(
            file.macros.len(),
            2,
            "one unreadable step must not cost the whole file"
        );
        assert_eq!(file.macros[1].steps, vec![MacroStep::Clear {}]);

        let steps = &file.macros[0].steps;
        assert_eq!(
            steps[0],
            MacroStep::Send {
                text: "show version".to_string(),
                newline: true,
            }
        );
        assert_eq!(steps[1].unknown_kind(), Some("http_request"));
        assert_eq!(
            steps[2].unknown_kind(),
            Some("send"),
            "a field this build does not know may change what the step does, so the step is not run without it"
        );
        let MacroStep::Repeat { count, steps: body } = &steps[3] else {
            panic!("the repeat itself is readable: {:?}", steps[3]);
        };
        assert_eq!(*count, 2);
        assert_eq!(body[0], MacroStep::Sleep { ms: 100 });
        assert_eq!(body[1].unknown_kind(), Some("beep"));
        assert_eq!(steps[4].unknown_kind(), Some("if"));
        assert_eq!(
            steps[5].unknown_kind(),
            Some("clear"),
            "a field added to a kind without fields must not be dropped either"
        );
        assert_eq!(steps[1].kind_label(), "Unknown Step");
        assert_eq!(
            steps[1].summary(),
            "Unknown step “http_request” (this version can't run it)"
        );

        let written = serialize_sessions_file(&file).expect("serializes");
        let original: serde_json::Value =
            serde_json::from_str(FUTURE_SESSIONS_FILE).expect("the fixture is plain JSON");
        let rewritten: serde_json::Value =
            serde_json::from_str(&written).expect("the output is plain JSON");
        let original_steps = &original["macros"][0]["steps"];
        let rewritten_steps = &rewritten["macros"][0]["steps"];
        for index in [1, 2, 4, 5] {
            assert_eq!(
                rewritten_steps[index], original_steps[index],
                "step {index} has to go back into the file untouched"
            );
        }
        assert_eq!(
            rewritten_steps[3]["steps"][1],
            original_steps[3]["steps"][1]
        );
        assert!(
            written.contains(r#""url": "https://example.invalid""#),
            "{written}"
        );
        assert_eq!(parse_sessions_file(&written), file);
    }

    #[test]
    fn a_repetition_right_after_a_reference_is_found() {
        assert_eq!(
            references_followed_by_repetition(r"^${p}+ ${q}* ${r}? ${s}{2} ${t}{x} ${u} (?:${v})+"),
            vec!["p", "q", "r", "s"]
        );
        assert_eq!(
            references_followed_by_repetition(r"$${p}+ ${p}+${p}+"),
            vec!["p"],
            "an escaped reference is text, and a name is reported once"
        );
        assert!(
            check_pattern_regex(r"(${p}+)").is_ok(),
            "the save-time check stands a one-character literal in, so it cannot see the problem"
        );
        let empty = HashMap::from([("p".to_string(), String::new())]);
        let broken = expand_variables_into_regex(r"(${p}+)", &empty);
        assert!(regex::Regex::new(&broken).is_err(), "{broken}");
        let grouped = expand_variables_into_regex(r"^(?:${p})+", &empty);
        assert!(
            regex::Regex::new(&grouped).is_ok(),
            "the form the warning suggests has to compile with an empty value: {grouped}"
        );
    }

    #[test]
    fn variables_are_expanded_and_unknown_names_are_left_alone() {
        let variables = HashMap::from([
            ("host".to_string(), "rt01".to_string()),
            ("empty".to_string(), String::new()),
        ]);

        assert_eq!(
            expand_variables("ping ${host} -c ${count}", &variables),
            "ping rt01 -c ${count}",
            "an undefined name stays visible instead of silently truncating the command"
        );
        assert_eq!(expand_variables("[${empty}]", &variables), "[]");
        assert_eq!(
            expand_variables("$${host}", &variables),
            "${host}",
            "$$ opens the escape for a literal reference"
        );
        assert_eq!(
            expand_variables("cost is $5 and $${x}", &variables),
            "cost is $5 and ${x}"
        );
        assert_eq!(
            expand_variables("${host", &variables),
            "${host",
            "an unterminated reference is text, not an error"
        );
        assert_eq!(expand_variables("${host}${host}", &variables), "rt01rt01");
        assert_eq!(expand_variables("", &variables), "");
    }

    #[test]
    fn a_value_spliced_into_a_regex_pattern_stands_for_itself() {
        let variables = HashMap::from([
            ("host".to_string(), "10.0.0.1".to_string()),
            ("error".to_string(), "unexpected ) here".to_string()),
        ]);

        assert_eq!(
            expand_variables_into_regex(r"^From ${host}:", &variables),
            r"^From 10\.0\.0\.1:",
            "the value is escaped, the anchor the user wrote around it is not"
        );
        assert_eq!(
            expand_variables(r"^From ${host}:", &variables),
            "^From 10.0.0.1:",
            "a send still gets the value as the user would type it"
        );
        assert_eq!(
            expand_variables_into_regex("${missing}", &variables),
            r"\$\{missing\}",
            "an unknown name is left as the text it is, which in a regex has to be escaped"
        );
        let unset = PatternMatcher::new(
            &expand_variables_into_regex(r"^From ${missing}:", &variables),
            true,
        )
        .expect("a reference nothing has set yet must not break the pattern");
        assert!(unset.find(&lines(&["From ${missing}:"])).is_some());
        assert!(unset.find(&lines(&["From rt01:"])).is_none());

        let matcher =
            PatternMatcher::new(&expand_variables_into_regex("${host}", &variables), true)
                .expect("compiles");
        assert!(matcher.find(&lines(&["ping 10.0.0.1"])).is_some());
        assert!(
            matcher.find(&lines(&["ping 10a0b0c1"])).is_none(),
            "an unescaped value would have matched this too, because . is any character"
        );

        assert!(
            PatternMatcher::new(&expand_variables_into_regex("${error}", &variables), true).is_ok(),
            "a captured value must not be able to turn a valid pattern into a broken one"
        );
    }

    #[test]
    fn a_regex_match_reports_its_named_groups() {
        let matcher =
            PatternMatcher::new(r"Version (?P<version>[^,]+), RELEASE (?P<image>\S+)", true)
                .expect("compiles");
        let found = matcher
            .find(&lines(&[
                "router#show version",
                "Cisco IOS Software, Version 15.2(4)E7, RELEASE c2960x",
            ]))
            .expect("the second line matches");
        assert_eq!(
            found.named_groups,
            vec![
                ("version".to_string(), "15.2(4)E7".to_string()),
                ("image".to_string(), "c2960x".to_string()),
            ]
        );
        assert_eq!(found.matched, "Version 15.2(4)E7, RELEASE c2960x");
        assert_eq!(
            found.line,
            "Cisco IOS Software, Version 15.2(4)E7, RELEASE c2960x"
        );

        let substring = PatternMatcher::new("Version", false).expect("compiles");
        let found = substring
            .find(&lines(&["banner", "IOS Version 15"]))
            .expect("matches");
        assert_eq!(
            found.matched, "IOS Version 15",
            "a substring pattern captures the line, because the search text itself says nothing new"
        );
        assert!(found.named_groups.is_empty());
    }

    #[test]
    fn a_repeat_step_summarizes_its_body() {
        assert_eq!(
            MacroStep::Repeat {
                count: 3,
                steps: vec![MacroStep::Clear {}],
            }
            .summary(),
            "Repeat: ×3 (1 step)"
        );
        assert_eq!(
            MacroStep::Repeat {
                count: 0,
                steps: Vec::new(),
            }
            .summary(),
            "Repeat: until stopped (0 steps)"
        );
    }

    /// The Japanese summaries read the way they did before the catalog, which
    /// is where the `${…}` escapes and the single plural form can go wrong.
    #[test]
    fn summaries_under_japanese_read_as_before_the_catalog() {
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(
            MacroStep::WaitFor {
                pattern: "router#".to_string(),
                regex: false,
                timeout_ms: 5_000,
                capture: Some("prompt".to_string()),
                on_timeout: Some(vec![MacroStep::Clear {}, MacroStep::Clear {}]),
            }
            .summary(),
            "待機: 『router#』 (5 秒以内)、${prompt} に取り込む、タイムアウト時 2 ステップ"
        );
        assert_eq!(
            MacroStep::If {
                pattern: "^15\\.".to_string(),
                regex: true,
                scope: MatchScope::Variable("ver".to_string()),
                capture: Some("banner".to_string()),
                then_steps: vec![MacroStep::Clear {}],
                else_steps: Vec::new(),
            }
            .summary(),
            "分岐: 『^15\\.』が${ver}にあれば 1 ステップ、なければ 0 ステップ、一致時は ${banner} に取り込む"
        );
        assert_eq!(
            MacroStep::If {
                pattern: "Invalid".to_string(),
                regex: false,
                scope: MatchScope::SinceLastSend,
                capture: None,
                then_steps: Vec::new(),
                else_steps: Vec::new(),
            }
            .summary(),
            "分岐: 『Invalid』が直前の出力にあれば 0 ステップ、なければ 0 ステップ"
        );
        assert_eq!(
            MacroStep::Repeat {
                count: 3,
                steps: vec![MacroStep::Clear {}],
            }
            .summary(),
            "繰り返し: 3 回 (1 ステップ)"
        );
        assert_eq!(
            MacroStep::SetVariable {
                name: "host".to_string(),
                value: "rt01".to_string(),
            }
            .summary(),
            "変数: ${host} に『rt01』"
        );
        assert_eq!(MacroStep::Clear {}.kind_label(), "画面を消去");
    }

    #[test]
    fn folders_and_tags_are_normalized() {
        assert_eq!(
            normalize_folder(" 拠点 / 本社 / "),
            Some("拠点/本社".to_string())
        );
        assert_eq!(normalize_folder("//"), None);
        assert_eq!(normalize_folder("   "), None);
        assert_eq!(normalize_folder("本社"), Some("本社".to_string()));
        assert_eq!(
            normalize_tags(" ルーター , 定期 ,, ルーター "),
            vec!["ルーター".to_string(), "定期".to_string()]
        );
        assert!(normalize_tags(" , ").is_empty());
    }

    #[test]
    fn zero_repeats_mean_until_cancelled() {
        let terminal_macro = TerminalMacro {
            repeat: 0,
            ..TerminalMacro::default()
        };
        assert_eq!(terminal_macro.iteration_count(), None);
    }

    #[test]
    fn an_unparsable_file_is_treated_as_empty() {
        assert_eq!(
            parse_sessions_file("{ not json"),
            TerminalSessionsFile::default()
        );
        assert_eq!(parse_sessions_file("  "), TerminalSessionsFile::default());
    }

    #[test]
    fn telnet_connections_use_the_host_port_form() {
        let connection = SavedConnection {
            name: "r1".into(),
            folder: None,
            protocol: ConnectionProtocol::Telnet,
            host: "10.0.0.1".into(),
            port: Some(2323),
            user: Some("admin".into()),
            macro_name: None,
            macro_folder: None,
            log: None,
            remember_password: false,
            transfer_protocol: None,
            transfer_port: None,
        };
        assert_eq!(
            connection.command(),
            ("telnet".to_string(), vec!["10.0.0.1".into(), "2323".into()])
        );
    }

    #[test]
    fn ssh_connections_include_the_user() {
        let connection = SavedConnection {
            name: "r1".into(),
            folder: None,
            protocol: ConnectionProtocol::Ssh,
            host: "10.0.0.1".into(),
            port: Some(2222),
            user: Some("admin".into()),
            macro_name: None,
            macro_folder: None,
            log: None,
            remember_password: false,
            transfer_protocol: None,
            transfer_port: None,
        };
        assert_eq!(
            connection.command(),
            (
                "ssh".to_string(),
                vec!["-p".into(), "2222".into(), "admin@10.0.0.1".into()]
            )
        );
    }

    #[test]
    fn new_output_is_the_lines_appended_since_the_baseline() {
        let baseline = lines(&["a", "b", "c"]);
        assert!(new_output_since(&baseline, &baseline).is_empty());
        assert_eq!(
            new_output_since(&baseline, &lines(&["a", "b", "c", "d"])),
            lines(&["d"])
        );
        // The window is full, so the oldest lines scrolled away.
        assert_eq!(
            new_output_since(&baseline, &lines(&["b", "c", "d"])),
            lines(&["d"])
        );
    }

    #[test]
    fn a_growing_last_line_counts_as_new_output() {
        let baseline = lines(&["login as:", "Passwo"]);
        assert_eq!(
            new_output_since(&baseline, &lines(&["login as:", "Password:"])),
            lines(&["Password:"])
        );
    }

    #[test]
    fn an_unalignable_screen_is_all_new_output() {
        assert_eq!(
            new_output_since(&lines(&["a", "b"]), &lines(&["x", "y"])),
            lines(&["x", "y"])
        );
        assert_eq!(new_output_since(&[], &lines(&["x"])), lines(&["x"]));
    }

    #[test]
    fn a_stale_prompt_does_not_satisfy_a_wait() {
        let baseline = lines(&["router#"]);
        let matcher = PatternMatcher::new("router#", false).expect("valid pattern");
        assert!(!matcher.matches(&new_output_since(&baseline, &baseline)));
        assert!(matcher.matches(&new_output_since(
            &baseline,
            &lines(&["router#", "show version", "router#"])
        )));
    }

    #[test]
    fn regex_patterns_match_per_line() {
        let matcher = PatternMatcher::new(r"^\S+[>#]\s*$", true).expect("valid pattern");
        assert!(matcher.matches(&lines(&["router#"])));
        assert!(!matcher.matches(&lines(&["not a prompt"])));
        assert!(PatternMatcher::new("[", true).is_err());
    }

    mod runner {
        use std::{cell::RefCell, rc::Rc};

        use gpui::TestAppContext;
        use util::paths::PathStyle;

        use super::*;
        use crate::{
            TerminalBuilder,
            terminal_settings::{AlternateScroll, CursorShape},
        };

        /// A terminal with no PTY, whose grid can be driven straight from the
        /// test with `write_output` (which emits the same `Event::Wakeup` the
        /// runner waits on).
        fn display_only_terminal(cx: &mut TestAppContext) -> Entity<Terminal> {
            cx.update(|cx| {
                let settings_store = settings::SettingsStore::test(cx);
                cx.set_global(settings_store);
            });
            cx.new(|cx| {
                TerminalBuilder::new_display_only(
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    PathStyle::local(),
                )
                .subscribe(cx)
            })
        }

        #[gpui::test]
        async fn wait_for_completes_when_the_text_appears(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            let progress = Rc::new(RefCell::new(Vec::new()));

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Sleep { ms: 1 },
                            MacroStep::WaitFor {
                                pattern: "READY".into(),
                                regex: false,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    {
                        let progress = progress.clone();
                        move |step, _| progress.borrow_mut().push(step.step_index)
                    },
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(10));
            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"READY\r\n", cx);
            });
            cx.run_until_parked();

            run.await.expect("the macro should finish");
            assert_eq!(*progress.borrow(), vec![0, 1]);
        }

        #[gpui::test]
        async fn a_repeated_macro_runs_its_steps_once_per_iteration(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            let progress = Rc::new(RefCell::new(Vec::new()));

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        repeat: 3,
                        repeat_interval_ms: 100,
                        steps: vec![
                            MacroStep::Send {
                                text: "show version".into(),
                                newline: true,
                            },
                            MacroStep::Clear {},
                        ],
                        ..TerminalMacro::default()
                    },
                    {
                        let progress = progress.clone();
                        move |step, _| {
                            progress.borrow_mut().push((
                                step.iteration,
                                step.step_index,
                                step.iteration_count,
                            ))
                        }
                    },
                    cx,
                )
            });

            cx.run_until_parked();
            // Two intervals separate the three iterations.
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(110));
                cx.run_until_parked();
            }

            run.await.expect("the macro should finish");
            assert_eq!(
                *progress.borrow(),
                vec![
                    (1, 0, Some(3)),
                    (1, 1, Some(3)),
                    (2, 0, Some(3)),
                    (2, 1, Some(3)),
                    (3, 0, Some(3)),
                    (3, 1, Some(3)),
                ]
            );
        }

        #[gpui::test]
        async fn an_endless_repeat_keeps_going_until_it_is_dropped(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            let iterations = Rc::new(RefCell::new(Vec::new()));

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        repeat: 0,
                        repeat_interval_ms: 100,
                        steps: vec![MacroStep::Send {
                            text: "show version".into(),
                            newline: true,
                        }],
                        ..TerminalMacro::default()
                    },
                    {
                        let iterations = iterations.clone();
                        move |step, _| {
                            iterations
                                .borrow_mut()
                                .push((step.iteration, step.iteration_count))
                        }
                    },
                    cx,
                )
            });

            cx.run_until_parked();
            for _ in 0..3 {
                cx.executor().advance_clock(Duration::from_millis(110));
                cx.run_until_parked();
            }
            // Dropping the task is how 中止 stops a run.
            drop(run);
            cx.run_until_parked();

            assert_eq!(
                *iterations.borrow(),
                vec![(1, None), (2, None), (3, None), (4, None)]
            );
        }

        #[gpui::test]
        async fn a_nested_repeat_runs_its_body_without_growing_the_outer_step_count(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);
            let progress = Rc::new(RefCell::new(Vec::new()));

            let send = |text: &str| MacroStep::Send {
                text: text.to_string(),
                newline: true,
            };

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            send("enable"),
                            MacroStep::Repeat {
                                count: 2,
                                steps: vec![send("show version"), MacroStep::Clear {}],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    {
                        let progress = progress.clone();
                        move |step, _| {
                            progress.borrow_mut().push((
                                step.step_index,
                                step.step_count,
                                step.nested.map(|nested| {
                                    (nested.iteration, nested.step_index, nested.kind_label)
                                }),
                            ))
                        }
                    },
                    cx,
                )
            });

            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                *progress.borrow(),
                vec![
                    (0, 2, None),
                    (1, 2, None),
                    (1, 2, Some((1, 0, "Send"))),
                    (1, 2, Some((1, 1, "Clear Screen"))),
                    (1, 2, Some((2, 0, "Send"))),
                    (1, 2, Some((2, 1, "Clear Screen"))),
                ]
            );
        }

        #[gpui::test]
        async fn a_failure_inside_a_nested_repeat_names_the_outer_and_inner_step(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "enable".into(),
                                newline: true,
                            },
                            MacroStep::Repeat {
                                count: 2,
                                steps: vec![MacroStep::WaitFor {
                                    pattern: "Password:".into(),
                                    regex: false,
                                    timeout_ms: 5_000,
                                    capture: None,
                                    on_timeout: None,
                                }],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(5_100));
            cx.run_until_parked();

            let error = run.await.expect_err("the nested wait should time out");
            assert_eq!(
                error.to_string(),
                "Step 2-1: “Password:” did not appear within 5 s"
            );
        }

        #[gpui::test]
        async fn an_endless_nested_repeat_keeps_going_until_it_is_dropped(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            let iterations = Rc::new(RefCell::new(Vec::new()));

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::Repeat {
                            count: 0,
                            steps: vec![MacroStep::Sleep { ms: 100 }],
                        }],
                        ..TerminalMacro::default()
                    },
                    {
                        let iterations = iterations.clone();
                        move |step, _| {
                            if let Some(nested) = step.nested {
                                iterations
                                    .borrow_mut()
                                    .push((nested.iteration, nested.iteration_count));
                            }
                        }
                    },
                    cx,
                )
            });

            cx.run_until_parked();
            for _ in 0..3 {
                cx.executor().advance_clock(Duration::from_millis(110));
                cx.run_until_parked();
            }
            // Dropping the task is how 中止 stops a run.
            drop(run);
            cx.run_until_parked();

            assert_eq!(
                *iterations.borrow(),
                vec![(1, None), (2, None), (3, None), (4, None)]
            );
        }

        #[gpui::test]
        async fn wait_for_times_out_and_names_the_step(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::WaitFor {
                            pattern: "Password:".into(),
                            regex: false,
                            timeout_ms: 5_000,
                            capture: None,
                            on_timeout: None,
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(5_100));
            cx.run_until_parked();

            let error = run.await.expect_err("the macro should time out");
            assert_eq!(
                error.to_string(),
                "Step 1: “Password:” did not appear within 5 s"
            );
        }

        #[gpui::test]
        async fn a_prompt_printed_before_the_send_does_not_satisfy_the_wait(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"router#\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show version".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: "router#".into(),
                                regex: false,
                                timeout_ms: 1_000,
                                capture: None,
                                on_timeout: None,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run.await.expect_err("the stale prompt must not match");
            assert!(
                error.to_string().starts_with("Step 2: “router#”"),
                "unexpected error: {error}"
            );
        }

        #[gpui::test]
        async fn a_branch_on_a_variable_decides_on_its_value(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            cx.run_until_parked();

            let branch = |variable: &str| MacroStep::If {
                pattern: r"^(?<major>\d+)\.".into(),
                regex: true,
                scope: MatchScope::Variable(variable.into()),
                capture: None,
                then_steps: vec![MacroStep::Send {
                    text: format!("{variable} is ${{major}}"),
                    newline: true,
                }],
                else_steps: vec![MacroStep::Send {
                    text: format!("{variable} did not match"),
                    newline: true,
                }],
            };

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::SetVariable {
                                name: "host".into(),
                                value: "rt01".into(),
                            },
                            MacroStep::SetVariable {
                                name: "ver".into(),
                                value: "15.2 on ${host}".into(),
                            },
                            MacroStep::SetVariable {
                                name: "empty".into(),
                                value: String::new(),
                            },
                            branch("ver"),
                            branch("empty"),
                            branch("never_set"),
                            MacroStep::If {
                                pattern: "^$".into(),
                                regex: true,
                                scope: MatchScope::Variable("empty".into()),
                                capture: None,
                                then_steps: vec![MacroStep::Send {
                                    text: "empty is empty".into(),
                                    newline: true,
                                }],
                                else_steps: Vec::new(),
                            },
                            MacroStep::Send {
                                text: "${ver}".into(),
                                newline: true,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    // The named group of the matching branch became a variable
                    // of its own, which is how part of a value is taken out.
                    "ver is 15".to_string(),
                    "empty did not match".to_string(),
                    // Nothing set it, so it matches nothing rather than the
                    // literal `${never_set}`.
                    "never_set did not match".to_string(),
                    "empty is empty".to_string(),
                    // A set value is expanded when it is set.
                    "15.2 on rt01".to_string(),
                ]
            );
        }

        #[gpui::test]
        async fn a_regex_naming_a_variable_not_set_yet_finds_nothing_instead_of_failing(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"From 10.0.0.1: bytes=32\r\n", cx);
            });
            cx.run_until_parked();

            let branch = MacroStep::If {
                pattern: r"^From ${host}: bytes".into(),
                regex: true,
                scope: MatchScope::Screen,
                capture: None,
                then_steps: vec![MacroStep::Send {
                    text: "reached ${host}".into(),
                    newline: true,
                }],
                else_steps: vec![MacroStep::Send {
                    text: "no answer from ${host}".into(),
                    newline: true,
                }],
            };
            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            branch.clone(),
                            MacroStep::SetVariable {
                                name: "host".into(),
                                value: "10.0.0.1".into(),
                            },
                            branch,
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            run.await
                .expect("an unset reference must not read as an invalid regex");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "no answer from ${host}".to_string(),
                    "reached 10.0.0.1".to_string(),
                ]
            );
        }

        #[gpui::test]
        async fn a_regex_that_an_empty_value_breaks_stops_the_run_on_that_step(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);
            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::SetVariable {
                                name: "p".into(),
                                value: String::new(),
                            },
                            MacroStep::If {
                                pattern: r"^(?:${p})+x".into(),
                                regex: true,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: Vec::new(),
                                else_steps: vec![MacroStep::Send {
                                    text: "grouped".into(),
                                    newline: true,
                                }],
                            },
                            MacroStep::If {
                                pattern: r"(${p}+)".into(),
                                regex: true,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: Vec::new(),
                                else_steps: Vec::new(),
                            },
                            MacroStep::Send {
                                text: "after".into(),
                                newline: true,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            let error = run
                .await
                .expect_err("`(+)` does not compile, and the run must say so");
            assert_eq!(
                error.to_string().lines().next(),
                Some("Step 3: the regular expression “(+)” is invalid (regex parse error:")
            );
            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["grouped".to_string()],
                "the steps after the broken one must not run"
            );
        }

        #[gpui::test]
        async fn a_macro_holding_a_step_this_build_cannot_read_is_not_run(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            let unknown = MacroStep::Unknown(serde_json::json!({ "type": "beep" }));
            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "configure terminal".into(),
                                newline: true,
                            },
                            MacroStep::If {
                                pattern: "x".into(),
                                regex: false,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: vec![MacroStep::Clear {}],
                                else_steps: vec![MacroStep::Clear {}, unknown],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            let error = run.await.expect_err("the macro must be refused");
            assert_eq!(
                error.to_string(),
                "Step 2-e2 is of a kind this version can't run (“beep”). The macro was not run."
            );
            assert!(
                sent_commands(&terminal, cx).is_empty(),
                "half a maintenance script is worse than none"
            );
        }

        /// What the macro typed, one entry per 送信, with the trailing CR cut
        /// off so the assertions read as commands.
        fn sent_commands(terminal: &Entity<Terminal>, cx: &mut TestAppContext) -> Vec<String> {
            terminal.update(cx, |terminal, _| {
                terminal
                    .take_pty_write_log()
                    .into_iter()
                    .map(|bytes| {
                        String::from_utf8_lossy(&bytes)
                            .trim_end_matches('\r')
                            .to_string()
                    })
                    .collect()
            })
        }

        #[gpui::test]
        async fn a_capture_feeds_the_sends_that_follow_it(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show version".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: r"Version (?P<version>\S+) on (?P<host>\S+)".into(),
                                regex: true,
                                timeout_ms: 60_000,
                                capture: Some("banner".into()),
                                on_timeout: None,
                            },
                            MacroStep::Send {
                                text: "echo ${host} runs ${version} [${banner}] ${missing} $${x}"
                                    .into(),
                                newline: true,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"Version 15.2 on rt01\r\n", cx);
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "show version".to_string(),
                    "echo rt01 runs 15.2 [Version 15.2 on rt01] ${missing} ${x}".to_string(),
                ]
            );
        }

        #[gpui::test]
        async fn a_substring_capture_stores_the_whole_line(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "hostname".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: "uptime is".into(),
                                regex: false,
                                timeout_ms: 60_000,
                                capture: Some("line".into()),
                                on_timeout: None,
                            },
                            MacroStep::Send {
                                text: "echo ${line}".into(),
                                newline: false,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"rt01 uptime is 3 days\r\n", cx);
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "hostname".to_string(),
                    "echo rt01 uptime is 3 days".to_string(),
                ]
            );
        }

        #[gpui::test]
        async fn a_branch_runs_the_side_the_output_picks(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show ip route".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: "rt01#".into(),
                                regex: false,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                            MacroStep::If {
                                pattern: "Invalid input".into(),
                                regex: false,
                                scope: MatchScope::SinceLastSend,
                                capture: None,
                                then_steps: vec![MacroStep::Send {
                                    text: "enable".into(),
                                    newline: true,
                                }],
                                else_steps: vec![MacroStep::Send {
                                    text: "write memory".into(),
                                    newline: true,
                                }],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"Invalid input detected\r\nrt01#", cx);
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["show ip route".to_string(), "enable".to_string()]
            );
        }

        #[gpui::test]
        async fn a_branch_sees_only_the_output_of_the_last_send_until_it_is_told_otherwise(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"Invalid input detected\r\nrt01#", cx);
            });
            cx.run_until_parked();

            let branch = |scope, then_text: &str, else_text: &str| MacroStep::If {
                pattern: "Invalid input".into(),
                regex: false,
                scope,
                capture: None,
                then_steps: vec![MacroStep::Send {
                    text: then_text.into(),
                    newline: true,
                }],
                else_steps: vec![MacroStep::Send {
                    text: else_text.into(),
                    newline: true,
                }],
            };

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show clock".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: "12:00".into(),
                                regex: false,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                            branch(MatchScope::SinceLastSend, "見えた", "見えない"),
                            branch(MatchScope::Screen, "画面で見えた", "画面で見えない"),
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"12:00:00 JST\r\n", cx);
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "show clock".to_string(),
                    // The banner is still on the grid, but it arrived before
                    // the 送信, so only the 画面全体 branch may see it.
                    "見えない".to_string(),
                    "画面で見えた".to_string(),
                ]
            );
        }

        #[gpui::test]
        async fn a_timeout_branch_carries_on_where_a_bare_wait_would_stop(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            let branches = Rc::new(RefCell::new(Vec::new()));

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "reload".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: "[confirm]".into(),
                                regex: false,
                                timeout_ms: 2_000,
                                capture: None,
                                on_timeout: Some(vec![MacroStep::Send {
                                    text: "諦めた".into(),
                                    newline: true,
                                }]),
                            },
                            MacroStep::Send {
                                text: "続き".into(),
                                newline: true,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    {
                        let branches = branches.clone();
                        move |progress, _| branches.borrow_mut().push(progress.branch)
                    },
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(2_100));
            cx.run_until_parked();
            run.await
                .expect("the timeout branch must not abort the run");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "reload".to_string(),
                    "諦めた".to_string(),
                    "続き".to_string(),
                ]
            );
            assert_eq!(
                *branches.borrow(),
                vec![None, None, Some(MacroBranch::Timeout), None],
                "the strip names the branch only while it runs"
            );
        }

        #[gpui::test]
        async fn a_branch_and_a_repeat_nest_both_ways_round(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            // Nothing the macro sends comes back as output (there is no PTY),
            // so the branches decide on what the test put on the grid.
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"Invalid input detected\r\n", cx);
            });
            cx.run_until_parked();

            let send = |text: &str| MacroStep::Send {
                text: text.to_string(),
                newline: true,
            };
            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::If {
                                pattern: "Invalid input".into(),
                                regex: false,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: vec![MacroStep::Repeat {
                                    count: 2,
                                    steps: vec![send("再実行")],
                                }],
                                else_steps: vec![send("そのまま")],
                            },
                            MacroStep::Repeat {
                                count: 2,
                                steps: vec![MacroStep::If {
                                    pattern: "現れない".into(),
                                    regex: false,
                                    scope: MatchScope::Screen,
                                    capture: None,
                                    then_steps: vec![send("あり")],
                                    else_steps: vec![send("なし")],
                                }],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "再実行".to_string(),
                    "再実行".to_string(),
                    "なし".to_string(),
                    "なし".to_string(),
                ]
            );
        }

        #[gpui::test]
        async fn a_failure_inside_a_branch_names_the_step_by_its_path(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Clear {},
                            MacroStep::If {
                                pattern: "現れない".into(),
                                regex: false,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: Vec::new(),
                                else_steps: vec![
                                    MacroStep::Clear {},
                                    MacroStep::WaitFor {
                                        pattern: "Password:".into(),
                                        regex: false,
                                        timeout_ms: 1_000,
                                        capture: None,
                                        on_timeout: None,
                                    },
                                ],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run.await.expect_err("the wait inside the branch times out");
            // 「それ以外」 is marked with an `e` so it cannot be confused with a
            // 「そのとき」 step that happens to sit at the same position.
            assert_eq!(
                error.to_string(),
                "Step 2-e2: “Password:” did not appear within 1 s"
            );
        }

        #[gpui::test]
        async fn captured_values_outlive_a_repetition(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        repeat: 2,
                        steps: vec![
                            MacroStep::If {
                                pattern: "現れない".into(),
                                regex: false,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: Vec::new(),
                                else_steps: vec![MacroStep::WaitFor {
                                    pattern: r"host=(?P<host>\S+)".into(),
                                    regex: true,
                                    timeout_ms: 500,
                                    capture: None,
                                    // The second pass finds nothing new to
                                    // match, and must keep the value anyway.
                                    on_timeout: Some(Vec::new()),
                                }],
                            },
                            MacroStep::Send {
                                text: "ping ${host}".into(),
                                newline: true,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"host=rt01\r\n", cx);
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(600));
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["ping rt01".to_string(), "ping rt01".to_string()]
            );
        }

        #[gpui::test]
        async fn a_captured_value_is_matched_as_text_by_a_later_pattern(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: r"addr=(?P<host>\S+)".into(),
                                regex: true,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                            MacroStep::WaitFor {
                                pattern: "${host}".into(),
                                regex: true,
                                timeout_ms: 1_000,
                                capture: None,
                                on_timeout: Some(vec![MacroStep::Send {
                                    text: "別物だった".into(),
                                    newline: true,
                                }]),
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"addr=10.0.0.1\r\n", cx);
            });
            cx.run_until_parked();
            // Every 10.0.0.1 has become 10a0b0c1 by the time the second wait
            // looks, so the wait may only be satisfied if the dots of the
            // captured address went in as dots.
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"10a0b0c1 answered\r\n", cx);
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();
            run.await.expect("the timeout branch carries the run on");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["show".to_string(), "別物だった".to_string()]
            );
        }

        #[gpui::test]
        async fn a_then_branch_failure_keeps_the_plain_step_number(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"Invalid input\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::If {
                            pattern: "Invalid input".into(),
                            regex: false,
                            scope: MatchScope::Screen,
                            capture: None,
                            then_steps: vec![MacroStep::WaitFor {
                                pattern: "現れない".into(),
                                regex: false,
                                timeout_ms: 1_000,
                                capture: None,
                                on_timeout: None,
                            }],
                            else_steps: Vec::new(),
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run
                .await
                .expect_err("the then branch's wait should time out");
            assert_eq!(
                error.to_string(),
                "Step 1-1: “現れない” did not appear within 1 s"
            );
        }

        #[gpui::test]
        async fn an_else_branch_failure_is_marked_with_e(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            // No 「Invalid input」 on screen, so the 「それ以外」 side runs.
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"rt01#\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::If {
                            pattern: "Invalid input".into(),
                            regex: false,
                            scope: MatchScope::Screen,
                            capture: None,
                            then_steps: Vec::new(),
                            else_steps: vec![MacroStep::WaitFor {
                                pattern: "現れない".into(),
                                regex: false,
                                timeout_ms: 1_000,
                                capture: None,
                                on_timeout: None,
                            }],
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run
                .await
                .expect_err("the else branch's wait should time out");
            // Before this the else side's step shared the plain 「1-1」 a then
            // step at the same position would also show, so a failure could
            // not say which side of the 分岐 it happened on.
            assert_eq!(
                error.to_string(),
                "Step 1-e1: “現れない” did not appear within 1 s"
            );
        }

        #[gpui::test]
        async fn a_timeout_branch_failure_is_marked_with_t(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::WaitFor {
                            pattern: "[confirm]".into(),
                            regex: false,
                            timeout_ms: 1_000,
                            capture: None,
                            on_timeout: Some(vec![MacroStep::WaitFor {
                                pattern: "現れない".into(),
                                regex: false,
                                timeout_ms: 1_000,
                                capture: None,
                                on_timeout: None,
                            }]),
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run
                .await
                .expect_err("the wait inside the timeout steps should also time out");
            assert_eq!(
                error.to_string(),
                "Step 1-t1: “現れない” did not appear within 1 s"
            );
        }

        #[gpui::test]
        async fn an_if_capture_feeds_the_then_branch_but_not_the_else_branch(
            cx: &mut TestAppContext,
        ) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"host=rt01\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::If {
                            pattern: r"host=(?P<host>\S+)".into(),
                            regex: true,
                            scope: MatchScope::Screen,
                            capture: Some("banner".into()),
                            then_steps: vec![MacroStep::Send {
                                text: "echo ${host} [${banner}]".into(),
                                newline: true,
                            }],
                            else_steps: vec![MacroStep::Send {
                                text: "not reached".into(),
                                newline: true,
                            }],
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["echo rt01 [host=rt01]".to_string()]
            );
        }

        #[gpui::test]
        async fn an_if_does_not_capture_when_the_else_branch_runs(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"no match here\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::If {
                            pattern: r"host=(?P<host>\S+)".into(),
                            regex: true,
                            scope: MatchScope::Screen,
                            capture: Some("banner".into()),
                            then_steps: vec![MacroStep::Send {
                                text: "unreachable".into(),
                                newline: true,
                            }],
                            else_steps: vec![MacroStep::Send {
                                text: "echo ${host} ${banner}".into(),
                                newline: true,
                            }],
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            run.await.expect("the macro should finish");

            // `${host}` (a named group) and `${banner}` (the `capture` field)
            // both stay literal: nothing was captured, because the 「それ以外」
            // side ran instead of the 「そのとき」 side.
            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["echo ${host} ${banner}".to_string()]
            );
        }

        #[gpui::test]
        async fn a_timeout_message_shows_the_captured_value_unescaped(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: r"addr=(?P<host>\S+)".into(),
                                regex: true,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                            MacroStep::WaitFor {
                                pattern: "${host}".into(),
                                regex: true,
                                timeout_ms: 1_000,
                                capture: None,
                                on_timeout: None,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"addr=10.0.0.1\r\n", cx);
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run.await.expect_err("the wait should time out");
            // The message must quote the address the way it was captured, not
            // the escaped form (`10\.0\.0\.1`) the regex was compiled with.
            assert_eq!(
                error.to_string(),
                "Step 3: “10.0.0.1” did not appear within 1 s"
            );
        }

        #[gpui::test]
        async fn an_if_capture_takes_a_japanese_variable_name(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output("認証に失敗しました\r\n".as_bytes(), cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::If {
                            // A 固定文字列 match, where what is filed is the
                            // whole line rather than the search text.
                            pattern: "失敗".into(),
                            regex: false,
                            scope: MatchScope::Screen,
                            // A multibyte name has to survive both the file and
                            // `${…}` expansion, which walks byte offsets.
                            capture: Some("理由".into()),
                            then_steps: vec![MacroStep::Send {
                                text: "echo ${理由}".into(),
                                newline: true,
                            }],
                            else_steps: Vec::new(),
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["echo 認証に失敗しました".to_string()]
            );
        }

        #[gpui::test]
        async fn a_wait_matches_a_japanese_pattern_and_files_it_unspaced(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "ログイン".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: r"エラー: (?P<reason>\S+)".into(),
                                regex: true,
                                timeout_ms: 60_000,
                                capture: Some("全文".into()),
                                on_timeout: None,
                            },
                            MacroStep::Send {
                                text: "echo ${reason} / ${全文}".into(),
                                newline: true,
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output("エラー: 認証失敗\r\n".as_bytes(), cx);
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec![
                    "ログイン".to_string(),
                    "echo 認証失敗 / エラー: 認証失敗".to_string(),
                ]
            );
        }

        /// The grid keeps a placeholder cell after every double-width character
        /// (and at the tail of a row too narrow to hold one), and both carry a
        /// space. Read back as text they turn 「認証」 into 「認 証」, which no
        /// pattern the user writes could ever match.
        #[gpui::test]
        async fn a_scanned_line_holds_no_spacer_cells(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            // Long enough to wrap, and starting on an odd column so a row ends
            // with a character that does not fit.
            let wrapped = format!("A{}", "認".repeat(100));
            terminal.update(cx, |terminal, cx| {
                terminal.write_output("認証に失敗しました\r\n".as_bytes(), cx);
                terminal.write_output(b"OK (ok)\r\n", cx);
                terminal.write_output(format!("{wrapped}\r\n").as_bytes(), cx);
            });
            cx.run_until_parked();

            let lines = terminal.update(cx, |terminal, _| terminal.last_n_non_empty_lines(10));
            assert_eq!(
                lines,
                vec![
                    "認証に失敗しました".to_string(),
                    // An ASCII line has no spacer cells to drop, so nothing
                    // else that scans the grid sees a difference.
                    "OK (ok)".to_string(),
                    wrapped,
                ]
            );
        }

        #[gpui::test]
        async fn a_branch_matches_a_captured_value_that_is_not_a_regex(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: r"addr=(?P<host>\S+)".into(),
                                regex: true,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                            MacroStep::If {
                                pattern: "${host}".into(),
                                regex: true,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: vec![MacroStep::Send {
                                    text: "見つかった".into(),
                                    newline: true,
                                }],
                                else_steps: vec![MacroStep::Send {
                                    text: "見つからない".into(),
                                    newline: true,
                                }],
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            // The unbalanced paren is the point: spliced in as written it is
            // not a regular expression at all, so the branch would stop the
            // macro instead of deciding anything.
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"addr=10.0.0.1(\r\n", cx);
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");

            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["show".to_string(), "見つかった".to_string()]
            );
        }

        #[gpui::test]
        async fn a_bad_regex_is_named_by_the_pattern_the_user_wrote(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![
                            MacroStep::Send {
                                text: "show".into(),
                                newline: true,
                            },
                            MacroStep::WaitFor {
                                pattern: r"addr=(?P<host>\S+)".into(),
                                regex: true,
                                timeout_ms: 60_000,
                                capture: None,
                                on_timeout: None,
                            },
                            MacroStep::If {
                                pattern: "${host}(".into(),
                                regex: true,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: Vec::new(),
                                else_steps: Vec::new(),
                            },
                        ],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"addr=10.0.0.1\r\n", cx);
            });
            cx.run_until_parked();

            let error = run
                .await
                .expect_err("the branch's pattern will not compile");
            let message = error.to_string();
            // The regex crate quotes the compiled form, where the captured
            // address is escaped, so the message has to lead with the pattern
            // as the user wrote it or there is nothing in it they recognise.
            assert!(
                message.starts_with("Step 3: the regular expression “10.0.0.1(” is invalid ("),
                "{message}"
            );
        }

        #[gpui::test]
        async fn three_levels_of_lists_are_all_spelled_in_the_step_number(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"rt01#\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![MacroStep::If {
                            pattern: "現れない".into(),
                            regex: false,
                            scope: MatchScope::Screen,
                            capture: None,
                            then_steps: Vec::new(),
                            else_steps: vec![MacroStep::If {
                                pattern: "これも現れない".into(),
                                regex: false,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: Vec::new(),
                                else_steps: vec![MacroStep::WaitFor {
                                    pattern: "待っても来ない".into(),
                                    regex: false,
                                    timeout_ms: 1_000,
                                    capture: None,
                                    on_timeout: Some(vec![MacroStep::WaitFor {
                                        pattern: "これも来ない".into(),
                                        regex: false,
                                        timeout_ms: 1_000,
                                        capture: None,
                                        on_timeout: None,
                                    }]),
                                }],
                            }],
                        }],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });

            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(1_100));
            cx.run_until_parked();

            let error = run.await.expect_err("the innermost wait times out");
            // Every level names the list it came from, so a 4-deep position is
            // still unambiguous.
            assert_eq!(
                error.to_string(),
                "Step 1-e1-e1-t1: “これも来ない” did not appear within 1 s"
            );
        }

        #[gpui::test]
        async fn an_empty_branch_pattern_matches_whatever_is_on_screen(cx: &mut TestAppContext) {
            // The editor refuses an empty 判定する文字列, so only a hand-edited
            // file gets here; what matters is that it decides instead of
            // stopping the macro.
            let terminal = display_only_terminal(cx);

            let empty_pattern_branch =
                |capture: Option<&str>, then: &str, otherwise: &str| MacroStep::If {
                    pattern: String::new(),
                    regex: false,
                    scope: MatchScope::Screen,
                    capture: capture.map(str::to_string),
                    then_steps: vec![MacroStep::Send {
                        text: then.to_string(),
                        newline: true,
                    }],
                    else_steps: vec![MacroStep::Send {
                        text: otherwise.to_string(),
                        newline: true,
                    }],
                };

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![empty_pattern_branch(None, "一致した", "何も無い")],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");
            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["何も無い".to_string()],
                "nothing on screen is nothing to find, even for an empty pattern"
            );

            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"rt01#\r\n", cx);
            });
            cx.run_until_parked();

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        steps: vec![empty_pattern_branch(Some("行"), "echo ${行}", "何も無い")],
                        ..TerminalMacro::default()
                    },
                    |_, _| {},
                    cx,
                )
            });
            cx.run_until_parked();
            run.await.expect("the macro should finish");
            assert_eq!(
                sent_commands(&terminal, cx),
                vec!["echo rt01#".to_string()],
                "an empty plain-text pattern sits in every line, so the first one is what \
                 gets filed"
            );
        }
    }
}
