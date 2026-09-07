//! Expect-style terminal macros and the saved connections that go with them.
//!
//! A macro is an ordered list of "send this line" / "wait until this text
//! appears" / "wait this long" steps, i.e. the subset of `pexpect` that router
//! and switch maintenance actually needs. Both macros and connections live in a
//! single user-owned JSON file (`terminal_sessions.json`) that is read with
//! comments tolerated and written atomically, the same way the egress policy
//! file is handled (DESIGN.md §4.3).

use std::{path::PathBuf, sync::Arc, time::Duration};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    },
    /// Block for a fixed duration.
    Sleep { ms: u64 },
    /// Wipe the screen and the scrollback, the way `terminal::Clear` does.
    Clear,
    /// Run `steps` `count` times before moving on. `0` repeats until the macro
    /// is cancelled, the same rule as [`TerminalMacro::repeat`].
    Repeat {
        #[serde(default = "default_repeat")]
        count: u32,
        #[serde(default)]
        steps: Vec<MacroStep>,
    },
}

impl MacroStep {
    /// Japanese label for the step kind, used by the editor's type dropdown.
    pub fn kind_label(&self) -> &'static str {
        match self {
            MacroStep::Send { .. } => "送信",
            MacroStep::WaitFor { .. } => "期待文字を待つ",
            MacroStep::Sleep { .. } => "秒待ち",
            MacroStep::Clear => "画面を消去",
            MacroStep::Repeat { .. } => "繰り返し",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            MacroStep::Send { text, .. } => format!("送信: {text}"),
            MacroStep::WaitFor {
                pattern,
                timeout_ms,
                ..
            } => format!(
                "待機: 『{pattern}』 ({} 秒以内)",
                *timeout_ms as f64 / 1000.0
            ),
            MacroStep::Sleep { ms } => format!("待機: {} 秒", *ms as f64 / 1000.0),
            MacroStep::Clear => "画面を消去".to_string(),
            MacroStep::Repeat { count, steps } => match count {
                0 => format!("繰り返し: 中止まで ({} ステップ)", steps.len()),
                count => format!("繰り返し: {count} 回 ({} ステップ)", steps.len()),
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
                .context("terminal_sessions.json を書き込めませんでした")
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
    let step_count = terminal_macro.steps.len();
    let iteration_count = terminal_macro.iteration_count();
    let interval = Duration::from_millis(terminal_macro.repeat_interval_ms);
    cx.spawn(async move |cx| {
        let mut runner = MacroRunner {
            terminal,
            on_progress: Box::new(on_progress),
            cx: cx.clone(),
            baseline: None,
            progress: MacroProgress {
                step_index: 0,
                step_count,
                iteration: 1,
                iteration_count,
                nested: None,
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
    progress: MacroProgress,
    /// Position of the running step, outermost first, so a failure inside a
    /// nested repeat can name 「ステップ 3-2」.
    path: Vec<usize>,
}

impl MacroRunner {
    fn report(&self) {
        let progress = self.progress;
        self.cx.update(|cx| (self.on_progress)(progress, cx));
    }

    /// 「3」 for a top-level step, 「3-2」 for the second step of the repeat that
    /// is the third step of the macro.
    fn step_label(&self) -> String {
        self.path
            .iter()
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Runs the outermost step list, which is the one the progress strip counts.
    async fn run_steps(&mut self, steps: &[MacroStep]) -> Result<()> {
        for (index, step) in steps.iter().enumerate() {
            self.progress.step_index = index;
            self.progress.nested = None;
            self.path.push(index);
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
                    self.path.push(index);
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

    async fn run_step(&mut self, step: &MacroStep) -> Result<()> {
        match step {
            MacroStep::Send { text, newline } => {
                self.baseline = Some(scan_lines(&self.terminal, &mut self.cx));
                let mut bytes = text.clone().into_bytes();
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
            MacroStep::Clear => {
                // The screen a wait looks at is gone, so the next wait has to
                // start from whatever the cleared grid holds.
                self.terminal
                    .update(&mut self.cx, |terminal, _| terminal.clear());
                self.baseline = None;
            }
            MacroStep::Repeat { count, steps } => {
                self.run_repeat(*count, steps).await?;
            }
            MacroStep::WaitFor {
                pattern,
                regex,
                timeout_ms,
            } => {
                let matcher = PatternMatcher::new(pattern, *regex).map_err(|error| {
                    anyhow!(
                        "ステップ {}: 正規表現が不正です ({error})",
                        self.step_label()
                    )
                })?;
                match wait_for_pattern(
                    &self.terminal,
                    &matcher,
                    self.baseline.as_deref(),
                    Duration::from_millis(*timeout_ms),
                    &mut self.cx,
                )
                .await
                {
                    Ok(matched) => self.baseline = Some(matched),
                    Err(WaitFailure::TimedOut) => bail!(
                        "ステップ {}: 『{pattern}』が {} 秒以内に現れませんでした",
                        self.step_label(),
                        *timeout_ms as f64 / 1000.0
                    ),
                    Err(WaitFailure::TerminalGone) => bail!(
                        "ステップ {}: 『{pattern}』を待っている間にターミナルが終了しました",
                        self.step_label()
                    ),
                }
            }
        }
        Ok(())
    }
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

/// Waits until `matcher` hits the output that arrived after `baseline`.
/// Returns the tail at the moment of the match, to be used as the next baseline.
async fn wait_for_pattern(
    terminal: &Entity<Terminal>,
    matcher: &PatternMatcher,
    baseline: Option<&[String]>,
    timeout: Duration,
    cx: &mut AsyncApp,
) -> Result<Vec<String>, WaitFailure> {
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
        if matcher.matches(&scanned) {
            return Ok(current);
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

impl PatternMatcher {
    pub fn new(pattern: &str, regex: bool) -> Result<Self> {
        if regex {
            Ok(PatternMatcher::Regex(regex::Regex::new(pattern)?))
        } else {
            Ok(PatternMatcher::Substring(pattern.to_string()))
        }
    }

    pub fn matches(&self, lines: &[String]) -> bool {
        lines.iter().any(|line| match self {
            PatternMatcher::Substring(pattern) => line.contains(pattern.as_str()),
            PatternMatcher::Regex(regex) => regex.is_match(line),
        })
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
                timeout_ms: DEFAULT_WAIT_TIMEOUT_MS
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
        assert_eq!(file.macros[0].steps, vec![MacroStep::Clear]);
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
    fn a_repeat_step_summarizes_its_body() {
        assert_eq!(
            MacroStep::Repeat {
                count: 3,
                steps: vec![MacroStep::Clear],
            }
            .summary(),
            "繰り返し: 3 回 (1 ステップ)"
        );
        assert_eq!(
            MacroStep::Repeat {
                count: 0,
                steps: Vec::new(),
            }
            .summary(),
            "繰り返し: 中止まで (0 ステップ)"
        );
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
                            MacroStep::Clear,
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
                                steps: vec![send("show version"), MacroStep::Clear],
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
                    (1, 2, Some((1, 0, "送信"))),
                    (1, 2, Some((1, 1, "画面を消去"))),
                    (1, 2, Some((2, 0, "送信"))),
                    (1, 2, Some((2, 1, "画面を消去"))),
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
                "ステップ 2-1: 『Password:』が 5 秒以内に現れませんでした"
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
                "ステップ 1: 『Password:』が 5 秒以内に現れませんでした"
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
                error.to_string().starts_with("ステップ 2: 『router#』"),
                "unexpected error: {error}"
            );
        }
    }
}
