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
use futures::StreamExt as _;
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
}

impl MacroStep {
    /// Japanese label for the step kind, used by the editor's type dropdown.
    pub fn kind_label(&self) -> &'static str {
        match self {
            MacroStep::Send { .. } => "送信",
            MacroStep::WaitFor { .. } => "期待文字を待つ",
            MacroStep::Sleep { .. } => "秒待ち",
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
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalMacro {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub steps: Vec<MacroStep>,
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
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
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
    /// Whether to start a session log for this connection. `None` follows the
    /// `terminal.session_log.auto` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<bool>,
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
    #[serde(default)]
    pub macros: Vec<TerminalMacro>,
}

impl TerminalSessionsFile {
    pub fn macro_named(&self, name: &str) -> Option<&TerminalMacro> {
        self.macros
            .iter()
            .find(|terminal_macro| terminal_macro.name == name)
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
                let file = parse_sessions_file(&contents);
                if this
                    .update(cx, |this, cx| {
                        if this.file != file {
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
            fs,
            path,
            _watch: watch,
        }
    }

    pub fn file(&self) -> &TerminalSessionsFile {
        &self.file
    }

    pub fn macros(&self) -> &[TerminalMacro] {
        &self.file.macros
    }

    pub fn connections(&self) -> &[SavedConnection] {
        &self.file.connections
    }

    pub fn macro_named(&self, name: &str) -> Option<&TerminalMacro> {
        self.file.macro_named(name)
    }

    /// Replaces the whole file and writes it back. The in-memory copy is
    /// updated right away so the UI does not have to wait for the watch to
    /// round-trip.
    pub fn write(
        &mut self,
        file: TerminalSessionsFile,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.file = file;
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
}

/// Runs `terminal_macro` against `terminal`, step by step.
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
    cx.spawn(async move |cx| {
        // `None` means "no 送信 step has run yet", in which case a wait may
        // match text that was already on screen (the login banner of a terminal
        // the macro was launched against). Once a step has sent something, the
        // waits only look at output that arrived afterwards, so a stale prompt
        // cannot satisfy them instantly.
        let mut baseline: Option<Vec<String>> = None;

        for (index, step) in terminal_macro.steps.iter().enumerate() {
            cx.update(|cx| {
                on_progress(
                    MacroProgress {
                        step_index: index,
                        step_count,
                    },
                    cx,
                )
            });

            match step {
                MacroStep::Send { text, newline } => {
                    baseline = Some(scan_lines(&terminal, cx));
                    let mut bytes = text.clone().into_bytes();
                    if *newline {
                        // TeraTerm sends CR for Enter; LF puts PowerShell into
                        // its continuation mode.
                        bytes.push(b'\r');
                    }
                    terminal.update(cx, |terminal, _| terminal.write_init_command(bytes));
                }
                MacroStep::Sleep { ms } => {
                    cx.background_executor()
                        .timer(Duration::from_millis(*ms))
                        .await;
                }
                MacroStep::WaitFor {
                    pattern,
                    regex,
                    timeout_ms,
                } => {
                    let matcher = PatternMatcher::new(pattern, *regex).map_err(|error| {
                        anyhow!("ステップ {}: 正規表現が不正です ({error})", index + 1)
                    })?;
                    match wait_for_pattern(
                        &terminal,
                        &matcher,
                        baseline.as_deref(),
                        Duration::from_millis(*timeout_ms),
                        cx,
                    )
                    .await
                    {
                        Ok(matched) => baseline = Some(matched),
                        Err(WaitFailure::TimedOut) => bail!(
                            "ステップ {}: 『{pattern}』が {} 秒以内に現れませんでした",
                            index + 1,
                            *timeout_ms as f64 / 1000.0
                        ),
                        Err(WaitFailure::TerminalGone) => bail!(
                            "ステップ {}: 『{pattern}』を待っている間にターミナルが終了しました",
                            index + 1
                        ),
                    }
                }
            }
        }

        Ok(())
    })
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
                protocol: ConnectionProtocol::Telnet,
                host: "192.168.1.1".to_string(),
                port: None,
                user: None,
                macro_name: Some("ログイン".to_string()),
                log: Some(true),
            }],
            macros: vec![TerminalMacro {
                name: "ログイン".to_string(),
                description: "特権モードまで進む".to_string(),
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
            protocol: ConnectionProtocol::Telnet,
            host: "10.0.0.1".into(),
            port: Some(2323),
            user: Some("admin".into()),
            macro_name: None,
            log: None,
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
            protocol: ConnectionProtocol::Ssh,
            host: "10.0.0.1".into(),
            port: Some(2222),
            user: Some("admin".into()),
            macro_name: None,
            log: None,
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
                        description: String::new(),
                        steps: vec![
                            MacroStep::Sleep { ms: 1 },
                            MacroStep::WaitFor {
                                pattern: "READY".into(),
                                regex: false,
                                timeout_ms: 60_000,
                            },
                        ],
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
        async fn wait_for_times_out_and_names_the_step(cx: &mut TestAppContext) {
            let terminal = display_only_terminal(cx);

            let run = cx.update(|cx| {
                run_macro(
                    terminal.clone(),
                    TerminalMacro {
                        name: "t".into(),
                        description: String::new(),
                        steps: vec![MacroStep::WaitFor {
                            pattern: "Password:".into(),
                            regex: false,
                            timeout_ms: 5_000,
                        }],
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
                        description: String::new(),
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
