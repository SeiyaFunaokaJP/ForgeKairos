//! TeraTerm-style session logging: everything the PTY writes to the terminal is
//! duplicated into a plain text file.
//!
//! The tap sits on the PTY read side (see [`crate::alacritty::TeePty`]) rather
//! than on the terminal grid, so output that scrolls out of the scrollback
//! buffer is still recorded, and a log started before a long-running command
//! captures every byte of it.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Local};
use futures::StreamExt as _;
use gpui::BackgroundExecutor;
use parking_lot::Mutex;

/// How long the writer waits before committing a line that has not been
/// terminated by a newline. Interactive prompts (`Password:`) never end in a
/// newline, so without this they would only reach the file when logging stops.
const PARTIAL_LINE_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Characters Windows forbids in file names, plus the separators that would
/// turn a substituted title into a directory traversal.
const FILE_NAME_RESERVED: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

const MAX_FILE_NAME_COMPONENT: usize = 64;

/// Whether logged lines carry a timestamp prefix, shared with the writer task
/// so the toggle in the recording strip takes effect on the next line rather
/// than only on the next log.
#[derive(Clone, Debug, Default)]
pub struct TimestampFlag(Arc<AtomicBool>);

impl TimestampFlag {
    pub fn new(enabled: bool) -> Self {
        Self(Arc::new(AtomicBool::new(enabled)))
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }
}

impl PartialEq for TimestampFlag {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl Eq for TimestampFlag {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLogOptions {
    /// Filter ANSI/VT escape sequences so the file reads as plain text.
    pub strip_control_sequences: bool,
    /// Prefix every line with `[YYYY-MM-DD HH:MM:SS] `.
    pub timestamps: TimestampFlag,
}

impl Default for SessionLogOptions {
    fn default() -> Self {
        Self {
            strip_control_sequences: true,
            timestamps: TimestampFlag::default(),
        }
    }
}

/// What [`SessionLogTap::resume`] writes in place of the output it dropped, so
/// a gap in the log is visible rather than mysterious (ADR-F29).
pub const PAUSED_MARKER: &str = i18n::msg!("[password input omitted]");

/// Handle installed into the PTY reader. Cloning is cheap; every clone shares
/// the same sink, so the terminal can start and stop logging long after the PTY
/// was created.
#[derive(Clone, Default)]
pub struct SessionLogTap {
    sink: Arc<Mutex<Option<async_channel::Sender<Vec<u8>>>>>,
    /// While set, [`Self::record`] throws away what it reads. Used around a
    /// password write so a device that echoes it does not put it in the file
    /// (ADR-F29). Separate from the sink because the pause must not stop, or be
    /// stopped by, the log itself.
    paused: Arc<AtomicBool>,
}

impl SessionLogTap {
    /// Called from the PTY reader thread for every chunk read. Must never
    /// block: the channel is unbounded, so `try_send` only fails once the
    /// writer task is gone, in which case the tap uninstalls itself.
    pub(crate) fn record(&self, bytes: &[u8]) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        let mut sink = self.sink.lock();
        let Some(sender) = sink.as_ref() else {
            return;
        };
        match sender.try_send(bytes.to_vec()) {
            Ok(()) => {}
            Err(async_channel::TrySendError::Full(_)) => {}
            Err(async_channel::TrySendError::Closed(_)) => {
                log::debug!("terminal session log writer went away; detaching tap");
                *sink = None;
            }
        }
    }

    /// Stops recording without stopping the log. Every pause must be matched by
    /// a [`Self::resume`], or the log stays silent until it is restarted.
    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    /// Resumes recording, writing `PAUSED_MARKER` on its own line first. The
    /// leading break closes the prompt line the pause interrupted.
    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        let marker = i18n::tr(PAUSED_MARKER);
        self.record(format!("\r\n{marker}\r\n").as_bytes());
    }

    pub(crate) fn install(&self, sender: async_channel::Sender<Vec<u8>>) {
        // A pause belongs to the write it was taken for, never to a log started
        // afterwards; a leaked one would silently record nothing.
        self.paused.store(false, Ordering::Relaxed);
        *self.sink.lock() = Some(sender);
    }

    fn detach(&self) {
        *self.sink.lock() = None;
    }
}

/// A running session log. Dropping it stops the writer task, which flushes and
/// closes the file.
pub struct SessionLog {
    path: PathBuf,
    options: SessionLogOptions,
    tap: SessionLogTap,
    /// `None` only while [`Drop`] hands the task off. Detached rather than
    /// dropped on stop, so the writer sees the closed channel, drains what is
    /// still queued and flushes the final partial line.
    writer: Option<gpui::Task<()>>,
}

impl SessionLog {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn options(&self) -> &SessionLogOptions {
        &self.options
    }

    /// The live flag the writer reads, so the recording strip can flip
    /// timestamps on and off without restarting the log.
    pub fn timestamps(&self) -> &TimestampFlag {
        &self.options.timestamps
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // Order matters: closing the channel first is what makes the detached
        // writer finish instead of running forever.
        self.tap.detach();
        if let Some(writer) = self.writer.take() {
            writer.detach();
        }
    }
}

/// Opens `path` for appending and starts the writer task.
///
/// The file is opened before this returns so a bad path (missing permissions,
/// unwritable directory) surfaces as an error the caller can show, rather than
/// as a log that silently records nothing.
pub fn start(
    path: PathBuf,
    options: SessionLogOptions,
    tap: SessionLogTap,
    executor: &BackgroundExecutor,
) -> Result<SessionLog> {
    let file = open_log_file(&path)?;
    let (sender, receiver) = async_channel::unbounded();
    tap.install(sender);

    let writer = executor.spawn({
        let executor = executor.clone();
        let path = path.clone();
        let options = options.clone();
        async move {
            if let Err(error) = run_writer(file, receiver, options, executor).await {
                log::error!("terminal session log {}: {error:#}", path.display());
            }
        }
    });

    Ok(SessionLog {
        path,
        options,
        tap,
        writer: Some(writer),
    })
}

fn open_log_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

async fn run_writer(
    file: File,
    chunks: async_channel::Receiver<Vec<u8>>,
    options: SessionLogOptions,
    executor: BackgroundExecutor,
) -> Result<()> {
    let mut file = BufWriter::new(file);
    let mut formatter = SessionLogFormatter::new(options);
    // `async_channel::Receiver` is not `Unpin`, so it is pinned once here
    // rather than boxing a fresh future for every chunk.
    let mut chunks = Box::pin(chunks);

    loop {
        let mut next_chunk = chunks.next();
        let idle = executor.timer(PARTIAL_LINE_FLUSH_INTERVAL);
        match futures::future::select(&mut next_chunk, idle).await {
            futures::future::Either::Left((Some(chunk), _)) => {
                formatter.push(&chunk);
            }
            futures::future::Either::Left((None, _)) => break,
            futures::future::Either::Right(((), _)) => {
                formatter.flush_partial_line();
            }
        }
        file.write_all(&formatter.take_output())?;
        file.flush()?;
    }

    formatter.finish();
    file.write_all(&formatter.take_output())?;
    file.flush()?;
    Ok(())
}

/// Turns raw PTY bytes into log file bytes.
struct SessionLogFormatter {
    parser: Option<vte::Parser>,
    lines: LineAssembler,
}

impl SessionLogFormatter {
    fn new(options: SessionLogOptions) -> Self {
        Self {
            parser: options.strip_control_sequences.then(vte::Parser::new),
            lines: LineAssembler::new(options.timestamps),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        match &mut self.parser {
            Some(parser) => parser.advance(&mut self.lines, bytes),
            None => self.lines.push_raw(bytes),
        }
    }

    fn flush_partial_line(&mut self) {
        self.lines.flush_partial_line();
    }

    fn finish(&mut self) {
        self.lines.finish();
    }

    /// Hands over the bytes accumulated since the last call. Draining is what
    /// makes the writer's per-chunk `write_all` append rather than re-write.
    fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.lines.output)
    }
}

/// Assembles output one line at a time so a line can still be rewritten by
/// backspace, and so a timestamp prefix can be stamped with the time the line
/// *started* rather than the time it ended.
struct LineAssembler {
    timestamps: TimestampFlag,
    now: Box<dyn Fn() -> DateTime<Local> + Send>,
    /// The line being built. Only the bytes past `flushed_len` still need writing.
    line: Vec<u8>,
    flushed_len: usize,
    line_started_at: Option<DateTime<Local>>,
    /// A carriage return that has not yet been resolved into a line break.
    pending_carriage_return: bool,
    output: Vec<u8>,
}

impl LineAssembler {
    fn new(timestamps: TimestampFlag) -> Self {
        Self::with_clock(timestamps, Box::new(Local::now))
    }

    fn with_clock(timestamps: TimestampFlag, now: Box<dyn Fn() -> DateTime<Local> + Send>) -> Self {
        Self {
            timestamps,
            now,
            line: Vec::new(),
            flushed_len: 0,
            line_started_at: None,
            pending_carriage_return: false,
            output: Vec::new(),
        }
    }

    fn push_raw(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                b'\n' => self.end_line(),
                b'\r' => self.pending_carriage_return = true,
                _ => self.push_byte(byte),
            }
        }
    }

    /// Resolves a deferred carriage return. `\r\n` is a line ending; a bare
    /// `\r` (progress bars, CR-only devices) also becomes a line break so the
    /// log never merges two unrelated lines onto one.
    fn resolve_carriage_return(&mut self) {
        if self.pending_carriage_return {
            self.pending_carriage_return = false;
            self.end_line();
        }
    }

    fn push_byte(&mut self, byte: u8) {
        self.resolve_carriage_return();
        if self.line.is_empty() {
            self.line_started_at = Some((self.now)());
        }
        self.line.push(byte);
    }

    fn push_str(&mut self, text: &str) {
        self.resolve_carriage_return();
        if self.line.is_empty() {
            self.line_started_at = Some((self.now)());
        }
        self.line.extend_from_slice(text.as_bytes());
    }

    /// Removes the last character of the current line, unless it was already
    /// written out by a partial flush.
    fn backspace(&mut self) {
        self.resolve_carriage_return();
        while self.line.len() > self.flushed_len {
            let byte = self.line.pop().unwrap_or(0);
            if !is_utf8_continuation(byte) {
                break;
            }
        }
    }

    /// Byte offset of `column` counted in characters from the start of the
    /// line, or the end of the line when it is shorter than that.
    ///
    /// A text log has no cell grid, so a column is taken to be one character
    /// wide even where the terminal would have drawn it two cells wide.
    fn byte_offset_of_column(&self, column: usize) -> usize {
        let mut characters = 0;
        for (offset, byte) in self.line.iter().enumerate() {
            if is_utf8_continuation(*byte) {
                continue;
            }
            if characters == column {
                return offset;
            }
            characters += 1;
        }
        self.line.len()
    }

    /// Moves the write position back to `column`, dropping what the line held
    /// past it so the redraw that follows replaces it rather than appending to
    /// it. This is what keeps line editors that repaint the whole input on
    /// every keystroke (PSReadLine, readline in bracketed paste) from leaving
    /// one fragment per keystroke in the log.
    ///
    /// Only the part of the line that is still pending can be taken back;
    /// bytes handed to the file by a partial flush stay as they are.
    fn move_to_column(&mut self, column: usize) {
        self.pending_carriage_return = false;
        let offset = self.byte_offset_of_column(column).max(self.flushed_len);
        self.line.truncate(offset);
    }

    /// Drops the whole pending part of the current line.
    fn erase_line(&mut self) {
        self.pending_carriage_return = false;
        self.line.truncate(self.flushed_len);
    }

    /// Moves the write position `count` characters to the left.
    fn move_left(&mut self, count: usize) {
        self.resolve_carriage_return();
        for _ in 0..count {
            if self.line.len() <= self.flushed_len {
                break;
            }
            self.backspace();
        }
    }

    fn end_line(&mut self) {
        self.pending_carriage_return = false;
        self.write_prefix_if_needed();
        let pending = self.line.len();
        self.output
            .extend_from_slice(&self.line[self.flushed_len..pending]);
        self.output.push(b'\n');
        self.line.clear();
        self.flushed_len = 0;
        self.line_started_at = None;
    }

    /// Emits whatever of the current line has not been written yet, without
    /// ending it. Further bytes on the same line append to the same output line.
    fn flush_partial_line(&mut self) {
        if self.line.len() == self.flushed_len {
            return;
        }
        self.write_prefix_if_needed();
        let pending = self.line.len();
        self.output
            .extend_from_slice(&self.line[self.flushed_len..pending]);
        self.flushed_len = pending;
    }

    fn write_prefix_if_needed(&mut self) {
        if !self.timestamps.get() || self.flushed_len != 0 {
            return;
        }
        let started_at = self.line_started_at.unwrap_or_else(|| (self.now)());
        self.output
            .extend_from_slice(timestamp_prefix(started_at).as_bytes());
    }

    fn finish(&mut self) {
        self.resolve_carriage_return();
        if self.line.len() > self.flushed_len {
            self.end_line();
        }
    }
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

fn timestamp_prefix(at: DateTime<Local>) -> String {
    at.format("[%Y-%m-%d %H:%M:%S] ").to_string()
}

impl vte::Perform for LineAssembler {
    fn print(&mut self, c: char) {
        let mut buffer = [0u8; 4];
        let encoded = c.encode_utf8(&mut buffer);
        // `encode_utf8` borrows `buffer`, so the write goes through a copy.
        let encoded = encoded.to_string();
        self.push_str(&encoded);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.end_line(),
            b'\r' => self.pending_carriage_return = true,
            b'\t' => self.push_byte(b'\t'),
            0x08 => self.backspace(),
            _ => {}
        }
    }

    /// Only the sequences that move within the current line or erase it are
    /// acted on. Everything else (colors, cursor visibility, scroll regions,
    /// movement between rows) has no meaning in a file of plain text lines and
    /// is dropped, which is the point of stripping in the first place.
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // A non-empty intermediate marks a private sequence such as
        // `ESC [ ? 25 l`, whose final byte collides with the ones below.
        if !intermediates.is_empty() {
            return;
        }
        let mut parameters = params.iter();
        let first = parameters
            .next()
            .and_then(|subparams| subparams.first())
            .copied()
            .unwrap_or(0);
        match action {
            // EL. Erasing to the right of the write position covers nothing,
            // because the assembler never holds anything past it.
            'K' => match first {
                0 => {}
                _ => self.erase_line(),
            },
            // CHA and HPA both take a one-based column.
            'G' | '`' => self.move_to_column(usize::from(first.max(1)) - 1),
            // CUP and HVP. The row is ignored: the assembler has no grid to
            // move around in, so all that can be honoured is the column.
            'H' | 'f' => {
                let column = parameters
                    .next()
                    .and_then(|subparams| subparams.first())
                    .copied()
                    .unwrap_or(1);
                self.move_to_column(usize::from(column.max(1)) - 1);
            }
            // CUB.
            'D' => self.move_left(usize::from(first.max(1))),
            _ => {}
        }
    }
}

/// Fields available to [`expand_file_name_template`].
#[derive(Default)]
pub struct SessionLogNameContext<'a> {
    pub title: &'a str,
    pub host: Option<&'a str>,
    pub process_id: Option<u32>,
}

/// Substitutes `{date}`, `{time}`, `{title}`, `{host}` and `{pid}` in a file
/// name template. Substituted values are sanitized, the template's own literal
/// text is not (so a user can keep subdirectories in the template).
pub fn expand_file_name_template(
    template: &str,
    at: DateTime<Local>,
    context: &SessionLogNameContext<'_>,
) -> String {
    let mut result = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            result.push_str(&rest[open..]);
            return result;
        };
        let name = &after[..close];
        match name {
            "date" => result.push_str(&at.format("%Y-%m-%d").to_string()),
            "time" => result.push_str(&at.format("%H%M%S").to_string()),
            "title" => result.push_str(&sanitize_file_name_component(context.title)),
            "host" => result.push_str(&sanitize_file_name_component(
                context.host.unwrap_or_default(),
            )),
            "pid" => result.push_str(
                &context
                    .process_id
                    .map(|pid| pid.to_string())
                    .unwrap_or_default(),
            ),
            _ => {
                result.push('{');
                result.push_str(name);
                result.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    result.push_str(rest);
    result
}

/// Makes `value` safe to embed in a file name on every supported platform.
pub fn sanitize_file_name_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut last_was_replacement = false;
    for character in value.chars() {
        let keep = !character.is_control()
            && !FILE_NAME_RESERVED.contains(&character)
            && character != '\u{7f}';
        if keep {
            sanitized.push(character);
            last_was_replacement = false;
        } else if !last_was_replacement {
            sanitized.push('_');
            last_was_replacement = true;
        }
    }
    // Windows drops trailing dots and spaces from file names, which would make
    // the written path differ from the one reported to the user.
    let trimmed = sanitized
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
        .trim_start();
    let mut trimmed = trimmed.to_string();
    if trimmed.chars().count() > MAX_FILE_NAME_COMPONENT {
        trimmed = trimmed.chars().take(MAX_FILE_NAME_COMPONENT).collect();
    }
    if trimmed.is_empty() {
        "terminal".to_string()
    } else {
        trimmed
    }
}

/// The path a new log takes given the current `terminal.session_log` settings.
pub fn default_log_path(
    settings: &crate::terminal_settings::SessionLogSettings,
    title: &str,
    host: Option<&str>,
) -> PathBuf {
    let directory = resolve_log_directory(settings.directory.as_deref());
    let file_name = expand_file_name_template(
        &settings.file_name_template,
        Local::now(),
        &SessionLogNameContext {
            title,
            host,
            process_id: None,
        },
    );
    directory.join(file_name)
}

/// Resolves the configured log directory, expanding a leading `~`.
pub fn resolve_log_directory(configured: Option<&str>) -> PathBuf {
    let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return paths::logs_dir().join("terminal-sessions");
    };
    if let Some(rest) = configured.strip_prefix("~/").or_else(|| {
        configured
            .strip_prefix("~\\")
            .filter(|_| cfg!(target_os = "windows"))
    }) {
        util::paths::home_dir().join(rest)
    } else if configured == "~" {
        util::paths::home_dir().clone()
    } else {
        PathBuf::from(configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn fixed_clock() -> Box<dyn Fn() -> DateTime<Local> + Send> {
        Box::new(|| {
            Local
                .with_ymd_and_hms(2026, 9, 3, 14, 5, 6)
                .single()
                .unwrap_or_else(Local::now)
        })
    }

    fn strip(input: &[u8]) -> String {
        let mut formatter = SessionLogFormatter::new(SessionLogOptions {
            strip_control_sequences: true,
            timestamps: TimestampFlag::new(false),
        });
        formatter.push(input);
        formatter.finish();
        String::from_utf8_lossy(&formatter.take_output()).into_owned()
    }

    /// Everything the tap has queued, run through the formatter the writer
    /// would use, so the assertions read like the log file.
    fn drain(receiver: &async_channel::Receiver<Vec<u8>>) -> String {
        let mut recorded = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            recorded.extend_from_slice(&chunk);
        }
        strip(&recorded)
    }

    #[test]
    fn a_paused_tap_keeps_the_password_out_of_the_log() {
        let tap = SessionLogTap::default();
        let (sender, receiver) = async_channel::unbounded();
        tap.install(sender);

        tap.record(b"Password: ");
        tap.pause();
        // What a device that echoes its input sends back.
        tap.record(b"hunter2\r\n");
        tap.resume();
        tap.record(b"switch> \r\n");

        let logged = drain(&receiver);
        assert!(!logged.contains("hunter2"), "{logged:?}");
        assert!(logged.contains("Password:"), "{logged:?}");
        assert!(logged.contains(PAUSED_MARKER), "{logged:?}");
        assert!(logged.contains("switch>"), "{logged:?}");
    }

    #[test]
    fn starting_a_log_clears_a_pause_left_behind_by_a_dropped_one() {
        let tap = SessionLogTap::default();
        tap.pause();

        let (sender, receiver) = async_channel::unbounded();
        tap.install(sender);
        tap.record(b"after the restart\r\n");

        assert!(drain(&receiver).contains("after the restart"));
    }

    #[test]
    fn strips_csi_and_osc_sequences() {
        assert_eq!(strip(b"\x1b[31mred\x1b[0m\r\n"), "red\n");
        assert_eq!(strip(b"\x1b]0;window title\x07prompt$ \r\n"), "prompt$ \n");
        assert_eq!(strip(b"\x1b[2J\x1b[H cleared\r\n"), " cleared\n");
    }

    #[test]
    fn normalizes_line_endings() {
        assert_eq!(strip(b"a\r\nb\r\n"), "a\nb\n");
        assert_eq!(strip(b"a\nb\n"), "a\nb\n");
        // A bare CR is an overwrite on screen; in a text log it becomes a break.
        assert_eq!(strip(b"50%\r100%\n"), "50%\n100%\n");
    }

    #[test]
    fn unterminated_line_is_flushed_at_finish() {
        assert_eq!(strip(b"Password: "), "Password: \n");
    }

    #[test]
    fn backspace_erases_the_previous_character() {
        assert_eq!(strip(b"abc\x08d\n"), "abd\n");
        assert_eq!(strip("あい\x08う\n".as_bytes()), "あう\n");
    }

    #[test]
    fn line_editor_redraws_leave_only_the_final_line() {
        // What PSReadLine sends for every keystroke: park the cursor at the
        // start of the input, repaint prompt and buffer, erase the leftovers.
        let mut redraws = Vec::new();
        for typed in ["e", "ec", "ech", "echo", "echo a", "echo ab"] {
            redraws.extend_from_slice(b"\x1b[1;1H");
            redraws.extend_from_slice(format!("PS> {typed}").as_bytes());
            redraws.extend_from_slice(b"\x1b[K");
        }
        redraws.extend_from_slice(b"\r\n");
        assert_eq!(strip(&redraws), "PS> echo ab\n");

        // The same thing written with CHA rather than CUP.
        assert_eq!(strip(b"echo a\x1b[1G\x1b[Kecho ab\r\n"), "echo ab\n");
        assert_eq!(strip(b"echo a\x1b[1`echo ab\r\n"), "echo ab\n");
    }

    #[test]
    fn erasing_to_the_right_keeps_the_text_before_the_cursor() {
        assert_eq!(strip(b"prompt$ \x1b[K\r\n"), "prompt$ \n");
        assert_eq!(strip(b"prompt$ \x1b[0K\r\n"), "prompt$ \n");
        assert_eq!(strip(b"prompt$ \x1b[2Kdone\r\n"), "done\n");
    }

    #[test]
    fn cursor_moves_within_the_line_truncate_it() {
        assert_eq!(strip(b"abcdef\x1b[4Gxy\r\n"), "abcxy\n");
        assert_eq!(strip(b"abcd\x1b[2Dxy\r\n"), "abxy\n");
        // A column past the end of the line leaves it alone.
        assert_eq!(strip(b"abc\x1b[9Gd\r\n"), "abcd\n");
        // Private sequences share their final byte and must not be mistaken
        // for cursor movement.
        assert_eq!(strip(b"ab\x1b[?25lcd\r\n"), "abcd\n");
    }

    #[test]
    fn a_redraw_cannot_take_back_a_flushed_line() {
        let mut formatter = SessionLogFormatter::new(SessionLogOptions {
            strip_control_sequences: true,
            timestamps: TimestampFlag::new(false),
        });
        formatter.push(b"echo a");
        formatter.flush_partial_line();
        formatter.push(b"\x1b[1G\x1b[Kecho ab\r\n");
        formatter.finish();
        assert_eq!(
            String::from_utf8_lossy(&formatter.take_output()),
            "echo aecho ab\n"
        );
    }

    #[test]
    fn raw_mode_keeps_escape_sequences() {
        let mut formatter = SessionLogFormatter::new(SessionLogOptions {
            strip_control_sequences: false,
            timestamps: TimestampFlag::new(false),
        });
        formatter.push(b"\x1b[31mred\x1b[0m\r\n");
        formatter.finish();
        assert_eq!(
            String::from_utf8_lossy(&formatter.take_output()),
            "\u{1b}[31mred\u{1b}[0m\n"
        );
    }

    #[test]
    fn taking_the_output_drains_it() {
        let mut formatter = SessionLogFormatter::new(SessionLogOptions {
            strip_control_sequences: true,
            timestamps: TimestampFlag::new(false),
        });
        formatter.push(b"one\r\n");
        assert_eq!(String::from_utf8_lossy(&formatter.take_output()), "one\n");
        formatter.push(b"two\r\n");
        assert_eq!(String::from_utf8_lossy(&formatter.take_output()), "two\n");
        formatter.finish();
        assert!(formatter.take_output().is_empty());
    }

    #[test]
    fn timestamps_prefix_each_line() {
        let mut lines = LineAssembler::with_clock(TimestampFlag::new(true), fixed_clock());
        lines.push_raw(b"one\r\ntwo\r\n");
        lines.finish();
        assert_eq!(
            String::from_utf8_lossy(&lines.output),
            "[2026-09-03 14:05:06] one\n[2026-09-03 14:05:06] two\n"
        );
    }

    #[test]
    fn toggling_timestamps_applies_from_the_next_line() {
        let flag = TimestampFlag::new(false);
        let mut lines = LineAssembler::with_clock(flag.clone(), fixed_clock());
        lines.push_raw(b"one\r\n");
        flag.set(true);
        lines.push_raw(b"two\r\n");
        flag.set(false);
        lines.push_raw(b"three\r\n");
        lines.finish();
        assert_eq!(
            String::from_utf8_lossy(&lines.output),
            "one\n[2026-09-03 14:05:06] two\nthree\n"
        );
    }

    #[test]
    fn partial_flush_does_not_duplicate_the_line() {
        let mut lines = LineAssembler::with_clock(TimestampFlag::new(true), fixed_clock());
        lines.push_raw(b"Passwo");
        lines.flush_partial_line();
        assert_eq!(
            String::from_utf8_lossy(&lines.output),
            "[2026-09-03 14:05:06] Passwo"
        );
        lines.push_raw(b"rd: \r\n");
        lines.finish();
        assert_eq!(
            String::from_utf8_lossy(&lines.output),
            "[2026-09-03 14:05:06] Password: \n"
        );
    }

    #[test]
    fn expands_the_file_name_template() {
        let at = Local
            .with_ymd_and_hms(2026, 9, 3, 14, 5, 6)
            .single()
            .expect("valid timestamp");
        let context = SessionLogNameContext {
            title: "rtx/1200:show",
            host: Some("192.168.1.1"),
            process_id: Some(4242),
        };
        assert_eq!(
            expand_file_name_template("{date}_{time}_{title}.log", at, &context),
            "2026-09-03_140506_rtx_1200_show.log"
        );
        assert_eq!(
            expand_file_name_template("{host}-{pid}.log", at, &context),
            "192.168.1.1-4242.log"
        );
        assert_eq!(
            expand_file_name_template("{unknown}.log", at, &context),
            "{unknown}.log"
        );
        assert_eq!(expand_file_name_template("{date", at, &context), "{date");
    }

    #[test]
    fn an_unset_directory_falls_back_to_the_log_folder() {
        let fallback = paths::logs_dir().join("terminal-sessions");
        assert_eq!(resolve_log_directory(None), fallback);
        // The settings default is an empty string rather than null, so that the
        // settings GUI has a default to offer and renders a text field.
        assert_eq!(resolve_log_directory(Some("")), fallback);
        assert_eq!(resolve_log_directory(Some("   ")), fallback);
        assert_eq!(
            resolve_log_directory(Some("~/logs")),
            util::paths::home_dir().join("logs")
        );
    }

    #[test]
    fn the_default_path_follows_the_configured_directory_and_template() {
        let settings = crate::terminal_settings::SessionLogSettings {
            auto: false,
            directory: Some("/var/log/zed".to_string()),
            file_name_template: "{title}.log".to_string(),
            strip_control_sequences: true,
            timestamps: false,
        };
        assert_eq!(
            default_log_path(&settings, "rtx/1200", None),
            PathBuf::from("/var/log/zed").join("rtx_1200.log")
        );
    }

    #[test]
    fn sanitizes_file_name_components() {
        assert_eq!(sanitize_file_name_component("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_file_name_component("trailing. "), "trailing");
        assert_eq!(sanitize_file_name_component(""), "terminal");
        assert_eq!(sanitize_file_name_component("../../etc"), ".._.._etc");
        assert_eq!(sanitize_file_name_component("ルーター"), "ルーター");
        assert_eq!(
            sanitize_file_name_component(&"x".repeat(200))
                .chars()
                .count(),
            MAX_FILE_NAME_COMPONENT
        );
    }
}
