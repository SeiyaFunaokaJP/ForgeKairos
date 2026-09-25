#[cfg(target_os = "windows")]
use std::num::NonZeroU32;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{borrow::Cow, io, ops::RangeInclusive, path::PathBuf, sync::Arc};

mod hyperlinks;

use alacritty_terminal::{
    event::{Event as AlacTermEvent, EventListener, Notify, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Grid, GridIterator, Row, Scroll as AlacScroll},
    index::{Boundary, Column, Direction as AlacDirection, Line, Point as AlacPoint},
    selection::{
        Selection as AlacSelection, SelectionRange as AlacSelectionRange,
        SelectionType as AlacSelectionType,
    },
    sync::FairMutex,
    term::{
        Config, Osc52, RenderableCursor, SEMANTIC_ESCAPE_CHARS, Term, TermMode,
        cell::{Cell as AlacCell, Flags, Hyperlink as AlacHyperlink},
        search::{Match, RegexIter, RegexSearch},
    },
    tty,
    vi_mode::{ViModeCursor, ViMotion as AlacViMotion},
    vte::ansi::{
        ClearMode, CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle,
        NamedPrivateMode, PrivateMode,
    },
};
use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use polling::{Event as PollingEvent, PollMode, Poller};
use util::paths::PathStyle;
use vte::ansi::Handler;
#[cfg(target_os = "windows")]
use windows::Win32::{Foundation::HANDLE, System::Threading::GetProcessId};

use crate::{
    Cell, Color, Content, Cursor, CursorShape, GridLinesChange, HoveredWord, Hyperlink,
    HyperlinkData, IndexedCell, Modes, Point, PtyEvent, Range, RenderableCells, Scroll, Search,
    Selection, SelectionRange, SelectionSide, SelectionType, TerminalBackendEvent, TerminalBounds,
    ViMotion,
    pty_info::ProcessIdGetter,
    remote_cwd::RemoteCwdTracker,
    remote_session::{FilteredOutput, ReplayMarkerFilter},
    session_log::SessionLogTap,
    terminal_settings::{AlternateScroll, CursorShape as SettingsCursorShape},
};

pub(super) use hyperlinks::{HyperlinkMatch, RegexSearches};

pub(super) type AlacrittyPty = tty::Pty;
pub(super) type AlacrittyTerm = Term<ZedListener>;
pub(super) type AlacrittyTermConfig = Config;
pub(super) type AlacrittyTermLock = FairMutex<AlacrittyTerm>;
pub(super) type AlacrittyCell = AlacCell;
pub(super) type AlacrittyGridIterator<'a> = GridIterator<'a, AlacCell>;
pub(super) type AlacrittyHyperlink = AlacHyperlink;

#[derive(Clone)]
pub(super) struct ZedListener(UnboundedSender<PtyEvent>);

#[derive(Clone, Debug)]
pub(super) struct AlacrittySearch {
    search: RegexSearch,
}

#[cfg(unix)]
impl From<&AlacrittyPty> for ProcessIdGetter {
    fn from(pty: &AlacrittyPty) -> Self {
        Self::new(pty.file().as_raw_fd(), pty.child().id())
    }
}

#[cfg(windows)]
impl From<&AlacrittyPty> for ProcessIdGetter {
    fn from(pty: &AlacrittyPty) -> Self {
        let child = pty.child_watcher();
        let handle = child.raw_handle();
        let fallback_pid = child.pid().unwrap_or_else(|| unsafe {
            NonZeroU32::new_unchecked(GetProcessId(HANDLE(handle as _)))
        });

        Self::new(handle as i32, u32::from(fallback_pid))
    }
}

pub(super) struct PtySender {
    notifier: Notifier,
}

impl PtySender {
    pub(super) fn notify(&self, input: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(input);
    }

    pub(super) fn resize(&self, bounds: TerminalBounds) {
        if let Err(error) = self
            .notifier
            .0
            .send(Msg::Resize(window_size_from_terminal_bounds(bounds)))
        {
            log::error!("failed to resize alacritty pty: {error}");
        }
    }

    pub(super) fn shutdown(&self) {
        if let Err(error) = self.notifier.0.send(Msg::Shutdown) {
            log::debug!("failed to shut down alacritty pty loop: {error}");
        }
    }
}

fn window_size_from_terminal_bounds(bounds: TerminalBounds) -> WindowSize {
    WindowSize {
        num_lines: bounds.num_lines() as u16,
        num_cols: bounds.num_columns() as u16,
        cell_width: f32::from(bounds.cell_width()) as u16,
        cell_height: f32::from(bounds.line_height()) as u16,
    }
}

pub(super) fn display_only_term_config(
    scrolling_history: usize,
    cursor_shape: SettingsCursorShape,
) -> AlacrittyTermConfig {
    Config {
        scrolling_history,
        default_cursor_style: alacritty_cursor_style(cursor_shape),
        semantic_escape_chars: format!("{SEMANTIC_ESCAPE_CHARS}─"),
        osc52: Osc52::Disabled,
        ..Config::default()
    }
}

pub(super) fn pty_term_config(
    scrolling_history: usize,
    cursor_shape: SettingsCursorShape,
) -> AlacrittyTermConfig {
    Config {
        scrolling_history,
        default_cursor_style: alacritty_cursor_style(cursor_shape),
        semantic_escape_chars: format!("{SEMANTIC_ESCAPE_CHARS}─"),
        ..Config::default()
    }
}

pub(super) fn set_default_cursor_style(
    config: &mut AlacrittyTermConfig,
    cursor_shape: SettingsCursorShape,
) {
    config.default_cursor_style = alacritty_cursor_style(cursor_shape);
}

pub(super) fn apply_config(term: &AlacrittyTermLock, config: &AlacrittyTermConfig) {
    term.lock().set_options(config.clone());
}

#[cfg(not(windows))]
pub(super) fn current_child_signal_mask() -> io::Result<tty::SignalMask> {
    tty::SignalMask::current()
}

pub(super) fn pty_options(
    shell: Option<(String, Vec<String>)>,
    working_directory: Option<PathBuf>,
    env: impl IntoIterator<Item = (String, String)>,
    #[cfg(not(windows))] child_signal_mask: Option<tty::SignalMask>,
    #[cfg(windows)] escape_args: bool,
) -> tty::Options {
    tty::Options {
        shell: shell.map(|(program, args)| tty::Shell::new(program, args)),
        working_directory,
        drain_on_exit: true,
        env: env.into_iter().collect(),
        #[cfg(not(windows))]
        child_signal_mask,
        #[cfg(windows)]
        escape_args,
    }
}

pub(super) fn open_pty(
    options: &tty::Options,
    bounds: TerminalBounds,
    window_id: u64,
) -> io::Result<AlacrittyPty> {
    tty::new(options, window_size_from_terminal_bounds(bounds), window_id)
}

pub(super) fn new_term(
    config: &AlacrittyTermConfig,
    bounds: TerminalBounds,
    events_tx: UnboundedSender<PtyEvent>,
    alternate_scroll: AlternateScroll,
) -> Arc<AlacrittyTermLock> {
    let mut term = Term::new(config.clone(), &bounds, ZedListener(events_tx));

    if let AlternateScroll::Off = alternate_scroll {
        term.unset_private_mode(PrivateMode::Named(NamedPrivateMode::AlternateScroll));
    }

    Arc::new(FairMutex::new(term))
}

/// Wraps a PTY so every byte read from it is also handed to a
/// [`SessionLogTap`], and — for remote shells — to a [`RemoteCwdTracker`].
/// A shell attached to a persistent session also has its replay markers
/// removed here, before any of the three sees them (see
/// [`crate::remote_session`]).
///
/// `EventedReadWrite::reader` hands back `&mut Self::Reader`, so a wrapper
/// cannot own a reader that borrows the inner PTY. Making the wrapper itself
/// the reader (`type Reader = Self`) sidesteps that without unsafe code: the
/// event loop's `pty.reader().read(..)` lands directly in the `io::Read` impl
/// below, which delegates to the inner reader and then tees.
pub(super) struct TeePty<P> {
    inner: P,
    tap: SessionLogTap,
    remote_cwd: Option<RemoteCwdTracker>,
    replay_markers: Option<ReplayMarkerFilter>,
    filtered: FilteredOutput,
    read_again: ReadAgain,
}

/// Brings alacritty's event loop back for output it left in the PTY.
///
/// The event loop stops reading after 64 KiB per wakeup (`MAX_LOCKED_READ`)
/// so it does not hold the terminal lock too long, and counts on the poller
/// to report the PTY readable again. On Unix the poller watches the PTY's
/// file descriptor level-triggered, so it does. On Windows the event loop
/// reads an in-process pipe that a thread fills from ConPTY, and that pipe
/// only posts a readable event to a reader that found it empty: a read that
/// takes data also takes back the reader's waker. After a stop at 64 KiB
/// with output still queued nothing posts again, the pipe fills up, the
/// thread stops reading ConPTY, and the shell blocks in its writes. A local
/// shell, `ssh.exe` and a remote shell behind it all stall like this under
/// fast output, until something unrelated (a key press) re-registers the
/// PTY. Posting a readable event after every read that handed out data
/// brings the loop back; when that later read finds the pipe empty, it
/// registers the waker the usual way.
#[derive(Default)]
struct ReadAgain {
    #[cfg(windows)]
    poller: Option<Arc<Poller>>,
}

#[cfg(windows)]
impl ReadAgain {
    fn set_poller(&mut self, poller: &Arc<Poller>, interest: PollingEvent) {
        self.poller = interest.readable.then(|| poller.clone());
    }

    fn clear(&mut self) {
        self.poller = None;
    }

    fn request(&self) {
        use polling::os::iocp::{CompletionPacket, PollerIocpExt as _};
        use util::ResultExt as _;

        if let Some(poller) = &self.poller {
            poller
                .post(CompletionPacket::new(PollingEvent::readable(
                    tty::PTY_READ_WRITE_TOKEN,
                )))
                .log_err();
        }
    }
}

#[cfg(not(windows))]
impl ReadAgain {
    fn set_poller(&mut self, _: &Arc<Poller>, _: PollingEvent) {}

    fn clear(&mut self) {}

    fn request(&self) {}
}

impl<P> TeePty<P> {
    fn new(
        inner: P,
        tap: SessionLogTap,
        remote_cwd: Option<RemoteCwdTracker>,
        replay_markers: Option<ReplayMarkerFilter>,
    ) -> Self {
        Self {
            inner,
            tap,
            remote_cwd,
            replay_markers,
            filtered: FilteredOutput::default(),
            read_again: ReadAgain::default(),
        }
    }
}

impl<P: tty::EventedReadWrite> io::Read for TeePty<P> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.read_and_tee(buf)?;
        if read > 0 {
            self.read_again.request();
        }
        Ok(read)
    }
}

impl<P: tty::EventedReadWrite> TeePty<P> {
    fn read_and_tee(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use io::Read as _;

        let Some(replay_markers) = self.replay_markers.as_mut() else {
            let read = self.inner.reader().read(buf)?;
            if read > 0 {
                self.tap.record(&buf[..read]);
                if let Some(remote_cwd) = &self.remote_cwd {
                    remote_cwd.record(&buf[..read]);
                }
            }
            return Ok(read);
        };

        loop {
            // What the filter holds back as the possible start of a marker
            // comes out in front of the next read, so that read has to leave
            // room for it. A remainder kept for a later call instead could sit
            // unseen until the shell next prints, because the event loop only
            // calls again when the PTY has something new.
            let room = buf.len().saturating_sub(replay_markers.pending_len());
            if room == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let read = self.inner.reader().read(&mut buf[..room])?;
            // What the filter holds back stays held back: on Windows `Ok(0)`
            // only means nothing more for now, and the rest of the marker may
            // still come. When the stream has really ended, the held bytes are
            // the unterminated start of an OSC, which the emulator would not
            // have shown either.
            if read == 0 {
                return Ok(0);
            }
            self.filtered.clear();
            replay_markers.filter(&buf[..read], &mut self.filtered);
            let length = self.filtered.bytes.len();
            // A chunk that was all marker must not come back as `Ok(0)`, which
            // the event loop reads as "nothing more for now" while the inner
            // PTY may still have data queued.
            if length == 0 {
                continue;
            }
            buf[..length].copy_from_slice(&self.filtered.bytes);
            for live in &self.filtered.live {
                self.tap.record(&self.filtered.bytes[live.clone()]);
            }
            if let Some(remote_cwd) = &self.remote_cwd {
                remote_cwd.record(&self.filtered.bytes);
            }
            return Ok(length);
        }
    }
}

impl<P: tty::EventedReadWrite> tty::EventedReadWrite for TeePty<P> {
    type Reader = Self;
    type Writer = P::Writer;

    unsafe fn register(
        &mut self,
        poller: &Arc<Poller>,
        interest: PollingEvent,
        mode: PollMode,
    ) -> io::Result<()> {
        self.read_again.set_poller(poller, interest);
        unsafe { self.inner.register(poller, interest, mode) }
    }

    fn reregister(
        &mut self,
        poller: &Arc<Poller>,
        interest: PollingEvent,
        mode: PollMode,
    ) -> io::Result<()> {
        self.read_again.set_poller(poller, interest);
        self.inner.reregister(poller, interest, mode)
    }

    fn deregister(&mut self, poller: &Arc<Poller>) -> io::Result<()> {
        self.read_again.clear();
        self.inner.deregister(poller)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        self
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.inner.writer()
    }
}

impl<P: tty::EventedPty> tty::EventedPty for TeePty<P> {
    fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
        self.inner.next_child_event()
    }
}

impl<P: alacritty_terminal::event::OnResize> alacritty_terminal::event::OnResize for TeePty<P> {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.inner.on_resize(window_size);
    }
}

pub(super) fn spawn_event_loop(
    term: Arc<AlacrittyTermLock>,
    events_tx: UnboundedSender<PtyEvent>,
    pty: AlacrittyPty,
    drain_on_exit: bool,
    session_log_tap: SessionLogTap,
    remote_cwd: Option<RemoteCwdTracker>,
    replay_markers: Option<ReplayMarkerFilter>,
) -> Result<PtySender> {
    let pty = TeePty::new(pty, session_log_tap, remote_cwd, replay_markers);
    let event_loop = EventLoop::new(term, ZedListener(events_tx), pty, drain_on_exit, false)
        .context("failed to create event loop")?;
    let pty_tx = event_loop.channel();
    let _io_thread = event_loop.spawn();

    Ok(PtySender {
        notifier: Notifier(pty_tx),
    })
}

pub(super) fn resize(term: &mut AlacrittyTerm, bounds: TerminalBounds) {
    term.resize(bounds);
}

pub(super) fn display_offset(term: &AlacrittyTerm) -> usize {
    term.grid().display_offset()
}

pub(super) fn scroll_display(term: &mut AlacrittyTerm, scroll: Scroll) {
    term.scroll_display(scroll.to_alacritty());
}

pub(super) fn set_selection(term: &mut AlacrittyTerm, selection: Option<&Selection>) {
    term.selection = selection.map(Selection::to_alacritty);
}

pub(super) fn update_selection(
    term: &mut AlacrittyTerm,
    point: Point,
    side: SelectionSide,
) -> bool {
    let Some(mut selection) = term.selection.take() else {
        return false;
    };
    selection.update(point.to_alacritty(), side.to_alacritty());
    term.selection = Some(selection);
    true
}

pub(super) fn selection_text(term: &AlacrittyTerm) -> Option<String> {
    term.selection_to_string()
}

pub(super) fn scroll_to_point(term: &mut AlacrittyTerm, point: Point) {
    term.scroll_to_point(point.to_alacritty());
}

pub(super) fn vi_goto_point(term: &mut AlacrittyTerm, point: Point) {
    term.vi_goto_point(point.to_alacritty());
}

pub(super) fn toggle_vi_mode(term: &mut AlacrittyTerm) {
    term.toggle_vi_mode();
}

pub(super) fn vi_motion(term: &mut AlacrittyTerm, motion: ViMotion) {
    term.vi_motion(motion.to_alacritty());
}

fn alacritty_cursor_style(cursor_shape: SettingsCursorShape) -> AlacCursorStyle {
    AlacCursorStyle {
        shape: alacritty_cursor_shape(cursor_shape),
        blinking: false,
    }
}

fn alacritty_cursor_shape(cursor_shape: SettingsCursorShape) -> AlacCursorShape {
    match cursor_shape {
        SettingsCursorShape::Block => AlacCursorShape::Block,
        SettingsCursorShape::Underline => AlacCursorShape::Underline,
        SettingsCursorShape::Bar => AlacCursorShape::Beam,
        SettingsCursorShape::Hollow => AlacCursorShape::HollowBlock,
    }
}

impl Dimensions for TerminalBounds {
    /// Note: this is supposed to be for the back buffer's length,
    /// but we exclusively use it to resize the terminal, which does not
    /// use this method. We still have to implement it for the trait though,
    /// hence, this comment.
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines()
    }

    fn columns(&self) -> usize {
        self.num_columns()
    }
}

impl From<AlacTermEvent> for TerminalBackendEvent {
    fn from(event: AlacTermEvent) -> Self {
        match event {
            AlacTermEvent::MouseCursorDirty => Self::MouseCursorDirty,
            AlacTermEvent::Title(title) => Self::Title(title),
            AlacTermEvent::ResetTitle => Self::ResetTitle,
            AlacTermEvent::ClipboardStore(_, data) => Self::ClipboardStore(data),
            AlacTermEvent::ClipboardLoad(_, format) => Self::ClipboardLoad(format),
            AlacTermEvent::ColorRequest(index, format) => Self::ColorRequest(index, format),
            AlacTermEvent::PtyWrite(output) => Self::PtyWrite(output),
            AlacTermEvent::TextAreaSizeRequest(format) => {
                Self::TextAreaSizeRequest(Arc::new(move |bounds| {
                    format(window_size_from_terminal_bounds(bounds))
                }))
            }
            AlacTermEvent::CursorBlinkingChange => Self::CursorBlinkingChange,
            AlacTermEvent::Wakeup => Self::Wakeup,
            AlacTermEvent::Bell => Self::Bell,
            AlacTermEvent::Exit => Self::Exit,
            AlacTermEvent::ChildExit(status) => Self::ChildExit(status),
        }
    }
}

impl EventListener for ZedListener {
    fn send_event(&self, event: AlacTermEvent) {
        self.0.unbounded_send(PtyEvent::Event(event.into())).ok();
    }
}

impl Scroll {
    fn to_alacritty(self) -> AlacScroll {
        match self {
            Self::Delta(delta) => AlacScroll::Delta(delta),
            Self::PageUp => AlacScroll::PageUp,
            Self::PageDown => AlacScroll::PageDown,
            Self::Top => AlacScroll::Top,
            Self::Bottom => AlacScroll::Bottom,
        }
    }
}

impl ViMotion {
    fn to_alacritty(self) -> AlacViMotion {
        match self {
            Self::Up => AlacViMotion::Up,
            Self::Down => AlacViMotion::Down,
            Self::Left => AlacViMotion::Left,
            Self::Right => AlacViMotion::Right,
            Self::First => AlacViMotion::First,
            Self::Last => AlacViMotion::Last,
            Self::FirstOccupied => AlacViMotion::FirstOccupied,
            Self::High => AlacViMotion::High,
            Self::Middle => AlacViMotion::Middle,
            Self::Low => AlacViMotion::Low,
            Self::WordLeft => AlacViMotion::WordLeft,
            Self::WordRight => AlacViMotion::WordRight,
            Self::WordRightEnd => AlacViMotion::WordRightEnd,
            Self::Bracket => AlacViMotion::Bracket,
            Self::ParagraphUp => AlacViMotion::ParagraphUp,
            Self::ParagraphDown => AlacViMotion::ParagraphDown,
        }
    }
}

impl Search {
    pub fn new(search: &str) -> Option<Self> {
        Some(Self {
            search: AlacrittySearch {
                search: RegexSearch::new(search).ok()?,
            },
        })
    }

    fn into_alacritty(self) -> RegexSearch {
        self.search.search
    }
}

impl SelectionSide {
    fn to_alacritty(self) -> AlacDirection {
        match self {
            Self::Left => AlacDirection::Left,
            Self::Right => AlacDirection::Right,
        }
    }
}

impl SelectionType {
    fn to_alacritty(self) -> AlacSelectionType {
        match self {
            Self::Simple => AlacSelectionType::Simple,
            Self::Semantic => AlacSelectionType::Semantic,
            Self::Lines => AlacSelectionType::Lines,
        }
    }
}

impl Selection {
    fn to_alacritty(&self) -> AlacSelection {
        let mut selection = AlacSelection::new(
            self.ty.to_alacritty(),
            self.start.point.to_alacritty(),
            self.start.side.to_alacritty(),
        );
        if self.start.point != self.end.point || self.start.side != self.end.side {
            selection.update(self.end.point.to_alacritty(), self.end.side.to_alacritty());
        }
        selection
    }
}

impl Hyperlink {
    pub fn new<T: ToString>(id: Option<T>, uri: String) -> Self {
        Self {
            data: HyperlinkData::Owned {
                id: id.map(|id| Arc::from(id.to_string())),
                uri: Arc::from(uri),
            },
        }
    }

    pub fn id(&self) -> Option<&str> {
        match &self.data {
            HyperlinkData::Alacritty(hyperlink) => Some(hyperlink.id()),
            HyperlinkData::Owned { id, .. } => id.as_deref(),
        }
    }

    pub fn uri(&self) -> &str {
        match &self.data {
            HyperlinkData::Alacritty(hyperlink) => hyperlink.uri(),
            HyperlinkData::Owned { uri, .. } => uri,
        }
    }

    fn from_alacritty(hyperlink: AlacHyperlink) -> Self {
        Self {
            data: HyperlinkData::Alacritty(hyperlink),
        }
    }
}

fn terminal_hyperlink_from_alacritty(hyperlink: AlacHyperlink) -> Hyperlink {
    Hyperlink::from_alacritty(hyperlink)
}

impl From<Hyperlink> for AlacHyperlink {
    fn from(hyperlink: Hyperlink) -> Self {
        match hyperlink.data {
            HyperlinkData::Alacritty(hyperlink) => hyperlink,
            HyperlinkData::Owned { id, uri } => Self::new(id.as_deref(), uri.to_string()),
        }
    }
}

fn terminal_cell_from_alacritty(cell: &AlacCell) -> Cell {
    Cell { cell: cell.clone() }
}

impl Cell {
    #[inline]
    pub fn character(&self) -> char {
        self.cell.c
    }

    #[cfg(test)]
    pub(crate) fn set_character(&mut self, character: char) {
        self.cell.c = character;
    }

    #[inline]
    pub fn foreground(&self) -> Color {
        self.cell.fg
    }

    #[inline]
    pub fn background(&self) -> Color {
        self.cell.bg
    }

    #[inline]
    pub fn zerowidth(&self) -> Option<&[char]> {
        self.cell.zerowidth()
    }

    #[cfg(test)]
    pub(crate) fn push_zerowidth(&mut self, character: char) {
        self.cell.push_zerowidth(character);
    }

    #[inline]
    pub fn hyperlink(&self) -> Option<Hyperlink> {
        self.cell.hyperlink().map(terminal_hyperlink_from_alacritty)
    }

    #[inline]
    pub fn is_inverse(&self) -> bool {
        self.cell.flags.contains(Flags::INVERSE)
    }

    #[inline]
    pub fn is_wide_char_spacer(&self) -> bool {
        self.cell.flags.contains(Flags::WIDE_CHAR_SPACER)
    }

    #[inline]
    pub fn is_dim(&self) -> bool {
        self.cell.flags.intersects(Flags::DIM)
    }

    #[inline]
    pub fn has_underline(&self) -> bool {
        self.cell.flags.intersects(Flags::ALL_UNDERLINES)
    }

    #[inline]
    pub fn has_undercurl(&self) -> bool {
        self.cell.flags.contains(Flags::UNDERCURL)
    }

    #[inline]
    pub fn has_strikeout(&self) -> bool {
        self.cell.flags.intersects(Flags::STRIKEOUT)
    }

    #[inline]
    pub fn is_bold(&self) -> bool {
        self.cell.flags.intersects(Flags::BOLD)
    }

    #[inline]
    pub fn is_italic(&self) -> bool {
        self.cell.flags.intersects(Flags::ITALIC)
    }

    #[inline]
    pub fn has_visible_style_modifier(&self) -> bool {
        self.cell
            .flags
            .intersects(Flags::ALL_UNDERLINES | Flags::INVERSE | Flags::STRIKEOUT)
    }
}

impl<'a> RenderableCells<'a> {
    pub(super) fn new(cells: GridIterator<'a, AlacCell>) -> Self {
        Self { cells }
    }
}

impl Iterator for RenderableCells<'_> {
    type Item = IndexedCell;

    fn next(&mut self) -> Option<Self::Item> {
        self.cells.next().map(|cell| IndexedCell {
            point: terminal_point_from_alacritty(cell.point),
            cell: terminal_cell_from_alacritty(cell.cell),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.cells.size_hint()
    }
}

impl Modes {
    #[cfg(test)]
    fn to_alacritty(self) -> TermMode {
        let mut mode = TermMode::empty();
        add_alacritty_mode(&mut mode, self, Self::APP_CURSOR, TermMode::APP_CURSOR);
        add_alacritty_mode(&mut mode, self, Self::APP_KEYPAD, TermMode::APP_KEYPAD);
        add_alacritty_mode(&mut mode, self, Self::SHOW_CURSOR, TermMode::SHOW_CURSOR);
        add_alacritty_mode(&mut mode, self, Self::LINE_WRAP, TermMode::LINE_WRAP);
        add_alacritty_mode(&mut mode, self, Self::ORIGIN, TermMode::ORIGIN);
        add_alacritty_mode(&mut mode, self, Self::INSERT, TermMode::INSERT);
        add_alacritty_mode(
            &mut mode,
            self,
            Self::LINE_FEED_NEW_LINE,
            TermMode::LINE_FEED_NEW_LINE,
        );
        add_alacritty_mode(&mut mode, self, Self::FOCUS_IN_OUT, TermMode::FOCUS_IN_OUT);
        add_alacritty_mode(
            &mut mode,
            self,
            Self::ALTERNATE_SCROLL,
            TermMode::ALTERNATE_SCROLL,
        );
        add_alacritty_mode(
            &mut mode,
            self,
            Self::BRACKETED_PASTE,
            TermMode::BRACKETED_PASTE,
        );
        add_alacritty_mode(&mut mode, self, Self::SGR_MOUSE, TermMode::SGR_MOUSE);
        add_alacritty_mode(&mut mode, self, Self::UTF8_MOUSE, TermMode::UTF8_MOUSE);
        add_alacritty_mode(&mut mode, self, Self::ALT_SCREEN, TermMode::ALT_SCREEN);
        add_alacritty_mode(
            &mut mode,
            self,
            Self::MOUSE_REPORT_CLICK,
            TermMode::MOUSE_REPORT_CLICK,
        );
        add_alacritty_mode(&mut mode, self, Self::MOUSE_DRAG, TermMode::MOUSE_DRAG);
        add_alacritty_mode(&mut mode, self, Self::MOUSE_MOTION, TermMode::MOUSE_MOTION);
        add_alacritty_mode(&mut mode, self, Self::VI, TermMode::VI);
        mode
    }
}

fn terminal_modes_from_alacritty(mode: TermMode) -> Modes {
    let mut terminal_modes = Modes::empty();
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::APP_CURSOR,
        Modes::APP_CURSOR,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::APP_KEYPAD,
        Modes::APP_KEYPAD,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::SHOW_CURSOR,
        Modes::SHOW_CURSOR,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::LINE_WRAP,
        Modes::LINE_WRAP,
    );
    add_terminal_mode(&mut terminal_modes, mode, TermMode::ORIGIN, Modes::ORIGIN);
    add_terminal_mode(&mut terminal_modes, mode, TermMode::INSERT, Modes::INSERT);
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::LINE_FEED_NEW_LINE,
        Modes::LINE_FEED_NEW_LINE,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::FOCUS_IN_OUT,
        Modes::FOCUS_IN_OUT,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::ALTERNATE_SCROLL,
        Modes::ALTERNATE_SCROLL,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::BRACKETED_PASTE,
        Modes::BRACKETED_PASTE,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::SGR_MOUSE,
        Modes::SGR_MOUSE,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::UTF8_MOUSE,
        Modes::UTF8_MOUSE,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::ALT_SCREEN,
        Modes::ALT_SCREEN,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::MOUSE_REPORT_CLICK,
        Modes::MOUSE_REPORT_CLICK,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::MOUSE_DRAG,
        Modes::MOUSE_DRAG,
    );
    add_terminal_mode(
        &mut terminal_modes,
        mode,
        TermMode::MOUSE_MOTION,
        Modes::MOUSE_MOTION,
    );
    add_terminal_mode(&mut terminal_modes, mode, TermMode::VI, Modes::VI);
    terminal_modes
}

fn add_terminal_mode(
    terminal_modes: &mut Modes,
    alacritty_modes: TermMode,
    alacritty_mode: TermMode,
    terminal_mode: Modes,
) {
    if alacritty_modes.contains(alacritty_mode) {
        terminal_modes.insert(terminal_mode);
    }
}

#[cfg(test)]
fn add_alacritty_mode(
    alacritty_modes: &mut TermMode,
    terminal_modes: Modes,
    terminal_mode: Modes,
    alacritty_mode: TermMode,
) {
    if terminal_modes.contains(terminal_mode) {
        alacritty_modes.insert(alacritty_mode);
    }
}

impl Cursor {
    fn from_alacritty(cursor: RenderableCursor) -> Self {
        Self {
            shape: terminal_cursor_shape_from_alacritty(cursor.shape),
            point: terminal_point_from_alacritty(cursor.point),
        }
    }
}

fn terminal_cursor_shape_from_alacritty(shape: AlacCursorShape) -> CursorShape {
    match shape {
        AlacCursorShape::Block => CursorShape::Block,
        AlacCursorShape::Underline => CursorShape::Underline,
        AlacCursorShape::Beam => CursorShape::Bar,
        AlacCursorShape::HollowBlock => CursorShape::HollowBlock,
        AlacCursorShape::Hidden => CursorShape::Hidden,
    }
}

impl Point {
    fn to_alacritty(self) -> AlacPoint {
        AlacPoint::new(Line(self.line), Column(self.column))
    }
}

fn terminal_point_from_alacritty(point: AlacPoint) -> Point {
    Point {
        line: point.line.0,
        column: point.column.0,
    }
}

impl Range {
    #[cfg(test)]
    fn to_alacritty(self) -> RangeInclusive<AlacPoint> {
        self.start.to_alacritty()..=self.end.to_alacritty()
    }

    fn from_alacritty(range: RangeInclusive<AlacPoint>) -> Self {
        Self {
            start: terminal_point_from_alacritty(*range.start()),
            end: terminal_point_from_alacritty(*range.end()),
        }
    }
}

fn terminal_selection_range_from_alacritty(range: AlacSelectionRange) -> SelectionRange {
    SelectionRange {
        start: terminal_point_from_alacritty(range.start),
        end: terminal_point_from_alacritty(range.end),
        is_block: range.is_block,
    }
}

pub(super) fn clear_saved_screen(term: &mut Term<ZedListener>) {
    term.clear_screen(ClearMode::Saved);

    let cursor = term.grid().cursor.point;

    term.grid_mut().reset_region(..cursor.line);

    let line = term.grid()[cursor.line][..Column(term.grid().columns())]
        .iter()
        .cloned()
        .enumerate()
        .collect::<Vec<(usize, AlacCell)>>();

    for (index, cell) in line {
        term.grid_mut()[Line(0)][Column(index)] = cell;
    }

    term.grid_mut().cursor.point = AlacPoint::new(Line(0), term.grid_mut().cursor.point.column);
    let new_cursor = term.grid().cursor.point;

    if (new_cursor.line.0 as usize) < term.screen_lines() - 1 {
        term.grid_mut().reset_region((new_cursor.line + 1)..);
    }
}

pub(super) fn shrink_to_used(term: &mut Term<ZedListener>) {
    term.grid_mut().truncate();
}

pub(super) fn used_lines(term: &Term<ZedListener>) -> usize {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return term.total_lines();
    }
    let grid = term.grid();
    let cursor_line = grid.cursor.point.line.0.max(0) as usize;
    let last_occupied_line = (cursor_line + 1..term.screen_lines())
        .rev()
        .find(|&line| !grid[Line(line as i32)].is_clear())
        .unwrap_or(cursor_line);
    term.history_size() + last_occupied_line + 1
}

fn adjusted_last_hovered_word(
    grid: &Grid<AlacCell>,
    last_content: &Content,
) -> (Option<HoveredWord>, GridLinesChange) {
    let grid_size_changed =
        grid.columns() != last_content.columns || grid.screen_lines() != last_content.screen_lines;

    let Some(total_lines_delta) = grid
        .total_lines()
        .checked_signed_diff(last_content.total_lines)
    else {
        return (None, GridLinesChange::Unchanged);
    };

    let Some(display_offset_delta) = grid
        .display_offset()
        .checked_signed_diff(last_content.display_offset)
    else {
        return (None, GridLinesChange::Unchanged);
    };

    let grid_visible_lines_changed = total_lines_delta != display_offset_delta;

    let grid_lines_change = if grid_size_changed || grid_visible_lines_changed {
        GridLinesChange::Changed
    } else {
        GridLinesChange::Unchanged
    };

    let hovered_word = if let Some(last_hovered_word) = last_content.last_hovered_word.as_ref()
        && grid_lines_change == GridLinesChange::Unchanged
    {
        if total_lines_delta == 0 {
            // Viewport and content are the same, we can reuse existing match
            Some(last_hovered_word.clone())
        } else {
            // Content is changed, but viewport is shifted the same, reuse
            // the match, but need to shift the match lines by the same amount.
            let mut adjusted_hovered_word = last_hovered_word.clone();
            adjusted_hovered_word.word_match = Range::new(
                Point::new(
                    last_hovered_word.word_match.start().line - display_offset_delta as i32,
                    last_hovered_word.word_match.start().column,
                ),
                Point::new(
                    last_hovered_word.word_match.end().line - display_offset_delta as i32,
                    last_hovered_word.word_match.end().column,
                ),
            );

            Some(adjusted_hovered_word)
        }
    } else {
        None
    };

    (hovered_word, grid_lines_change)
}

pub(super) fn make_content(term: &Term<ZedListener>, last_content: &Content) -> Content {
    let content = term.renderable_content();

    let estimated_size = content.display_iter.size_hint().0;
    let mut cells = Vec::with_capacity(estimated_size);

    cells.extend(content.display_iter.map(|ic| IndexedCell {
        point: terminal_point_from_alacritty(ic.point),
        cell: terminal_cell_from_alacritty(ic.cell),
    }));

    let selection_text = if content.selection.is_some() {
        term.selection_to_string()
    } else {
        None
    };

    let grid = term.grid();
    let (last_hovered_word, grid_lines_change) = adjusted_last_hovered_word(grid, last_content);

    let bottom_line = term.screen_lines() as i32 - 1 - content.display_offset as i32;
    let bottom_row_occupied = content.cursor.point.line.0 >= bottom_line
        || cells
            .iter()
            .rev()
            .take_while(|cell| cell.point.line >= bottom_line)
            .any(|cell| cell.cell.character() != ' ');

    Content {
        cells,
        mode: terminal_modes_from_alacritty(content.mode),
        total_lines: grid.total_lines(),
        display_offset: grid.display_offset(),
        columns: grid.columns(),
        screen_lines: grid.screen_lines(),
        selection_text,
        selection: content
            .selection
            .map(terminal_selection_range_from_alacritty),
        cursor: Cursor::from_alacritty(content.cursor),
        cursor_char: term.grid()[content.cursor.point].c,
        terminal_bounds: last_content.terminal_bounds,
        last_hovered_word,
        grid_lines_change,
        scrolled_to_top: content.display_offset == term.history_size(),
        scrolled_to_bottom: content.display_offset == 0,
        bottom_row_occupied,
    }
}

pub(super) fn content_text(term: &Term<ZedListener>) -> String {
    let start = AlacPoint::new(term.topmost_line(), Column(0));
    let end = AlacPoint::new(term.bottommost_line(), term.last_column());
    term.bounds_to_string(start, end)
}

pub(super) fn total_lines(term: &Term<ZedListener>) -> usize {
    term.total_lines()
}

pub(super) fn screen_lines(term: &Term<ZedListener>) -> usize {
    term.screen_lines()
}

pub(super) fn full_content_range(term: &Term<ZedListener>) -> Range {
    let start = AlacPoint::new(term.topmost_line(), Column(0));
    let end = AlacPoint::new(term.bottommost_line(), term.last_column());
    Range::from_alacritty(start..=end)
}

pub(super) fn last_non_empty_lines(term: &Term<ZedListener>, line_count: usize) -> Vec<String> {
    let grid = term.grid();
    let mut lines = Vec::new();

    let mut current_line = grid.bottommost_line().0;
    let topmost_line = grid.topmost_line().0;

    while current_line >= topmost_line && lines.len() < line_count {
        let (logical_line_start, logical_line) =
            logical_line_for_row(grid, current_line, topmost_line);

        if let Some(line) = process_line(logical_line) {
            lines.push(line);
        }

        current_line = logical_line_start - 1;
    }

    lines.reverse();
    lines
}

pub(super) fn update_vi_cursor_for_scroll(term: &mut Term<ZedListener>, scroll: Scroll) {
    match scroll {
        Scroll::Delta(delta) => {
            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, delta);
        }
        Scroll::PageUp => {
            let lines = term.screen_lines() as i32;
            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, lines);
        }
        Scroll::PageDown => {
            let lines = -(term.screen_lines() as i32);
            term.vi_mode_cursor = term.vi_mode_cursor.scroll(term, lines);
        }
        Scroll::Top => {
            let point = AlacPoint::new(term.topmost_line(), Column(0));
            term.vi_mode_cursor = ViModeCursor::new(point);
        }
        Scroll::Bottom => {
            let point = AlacPoint::new(term.bottommost_line(), Column(0));
            term.vi_mode_cursor = ViModeCursor::new(point);
        }
    }
}

pub(super) fn update_selection_to_vi_cursor(term: &mut Term<ZedListener>) -> Option<Point> {
    let mut selection = term.selection.take()?;
    let point = term.vi_mode_cursor.point;
    selection.update(point, AlacDirection::Right);
    term.selection = Some(selection);
    Some(terminal_point_from_alacritty(point))
}

pub(super) fn find_from_terminal_point(
    term: &AlacrittyTerm,
    point: Point,
    regex_searches: &mut RegexSearches,
    path_style: PathStyle,
) -> Option<HyperlinkMatch> {
    let point = point.to_alacritty().grid_clamp(term, Boundary::Grid);
    hyperlinks::find_from_grid_point(term, point, regex_searches, path_style)
}

fn logical_line_for_row(grid: &Grid<AlacCell>, current: i32, topmost: i32) -> (i32, String) {
    let start = find_logical_line_start(grid, current, topmost);
    let mut line = String::new();
    for row in start..=current {
        line.push_str(&row_to_string(&grid[Line(row)]));
    }
    (start, line)
}

fn find_logical_line_start(grid: &Grid<AlacCell>, current: i32, topmost: i32) -> i32 {
    let mut line_start = current;
    while line_start > topmost {
        let previous_line = Line(line_start - 1);
        let last_cell = &grid[previous_line][Column(grid.columns() - 1)];
        if !last_cell.flags.contains(Flags::WRAPLINE) {
            break;
        }
        line_start -= 1;
    }
    line_start
}

fn row_to_string(row: &Row<AlacCell>) -> String {
    row[..Column(row.len())]
        .iter()
        .filter(|cell| !is_wide_char_spacer(cell))
        .map(|cell| cell.c)
        .collect::<String>()
}

/// Whether a cell only holds a place rather than a character: the second half
/// of a double-width character, or the tail of a row too narrow to fit one (the
/// character itself then starts the wrapped row).
///
/// Both carry `' '` as their character, so reading a row cell by cell turns
/// 「認証」 into 「認 証」. Everything downstream of
/// [`last_non_empty_lines`] compares that text against something the user or a
/// device wrote — a macro's 期待文字 or 分岐, the Telnet prompt scan, the
/// startup marker, the lines handed to the inline assistant — so the spaces
/// have to go before the comparison, not after.
fn is_wide_char_spacer(cell: &AlacCell) -> bool {
    cell.flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

fn process_line(line: String) -> Option<String> {
    let trimmed = line.trim_end().to_string();
    if !trimmed.is_empty() {
        Some(trimmed)
    } else {
        None
    }
}

/// Appends a stringified task summary to the terminal, after its output.
///
/// SAFETY: This function should only be called after terminal's PTY is no longer alive.
/// New text being added to the terminal here, uses "less public" APIs,
/// which are not maintaining the entire terminal state intact.
///
///
/// The library
///
/// * does not increment inner grid cursor's _lines_ on `input` calls
///   (but displaying the lines correctly and incrementing cursor's columns)
///
/// * ignores `\n` and \r` character input, requiring the `newline` call instead
///
/// * does not alter grid state after `newline` call
///   so its `bottommost_line` is always the same additions, and
///   the cursor's `point` is not updated to the new line and column values
///
/// * ??? there could be more consequences, and any further "proper" streaming from the PTY might bug and/or panic.
///   Still, subsequent `append_text_to_term` invocations are possible and display the contents correctly.
///
/// Despite the quirks, this is the simplest approach to appending text to the terminal: its alternative, `grid_mut` manipulations,
/// do not properly set the scrolling state and display odd text after appending; also those manipulations are more tedious and error-prone.
/// The function achieves proper display and scrolling capabilities, at a cost of grid state not properly synchronized.
/// This is enough for printing moderately-sized texts like task summaries, but might break or perform poorly for larger texts.
pub(super) unsafe fn append_text_to_term(term: &mut Term<ZedListener>, text_lines: &[&str]) {
    term.newline();
    term.grid_mut().cursor.point.column = Column(0);
    for line in text_lines {
        for character in line.chars() {
            term.input(character);
        }
        term.newline();
        term.grid_mut().cursor.point.column = Column(0);
    }
}

pub(super) fn search_matches(term: &AlacrittyTerm, searcher: Search) -> Vec<Range> {
    let mut searcher = searcher.into_alacritty();
    all_search_matches(term, &mut searcher)
        .map(Range::from_alacritty)
        .collect()
}

fn all_search_matches<'a, T>(
    term: &'a Term<T>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let start = AlacPoint::new(term.grid().topmost_line(), Column(0));
    let end = AlacPoint::new(term.grid().bottommost_line(), term.grid().last_column());
    RegexIter::new(start, end, AlacDirection::Right, term, regex)
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::sync::Arc;

    use super::*;
    use alacritty_terminal::tty::EventedReadWrite as _;

    struct FakePty {
        reader: io::Cursor<Vec<u8>>,
        writer: Vec<u8>,
    }

    impl tty::EventedReadWrite for FakePty {
        type Reader = io::Cursor<Vec<u8>>;
        type Writer = Vec<u8>;

        unsafe fn register(
            &mut self,
            _: &Arc<Poller>,
            _: PollingEvent,
            _: PollMode,
        ) -> io::Result<()> {
            Ok(())
        }

        fn reregister(&mut self, _: &Arc<Poller>, _: PollingEvent, _: PollMode) -> io::Result<()> {
            Ok(())
        }

        fn deregister(&mut self, _: &Arc<Poller>) -> io::Result<()> {
            Ok(())
        }

        fn reader(&mut self) -> &mut Self::Reader {
            &mut self.reader
        }

        fn writer(&mut self) -> &mut Self::Writer {
            &mut self.writer
        }
    }

    #[test]
    fn tee_pty_duplicates_reads_only_while_a_tap_is_installed() {
        let tap = SessionLogTap::default();
        let mut pty = TeePty::new(
            FakePty {
                reader: io::Cursor::new(b"before-tapafter-tap".to_vec()),
                writer: Vec::new(),
            },
            tap.clone(),
            None,
            None,
        );

        let mut buffer = [0u8; 10];
        let read = pty.reader().read(&mut buffer).expect("read succeeds");
        assert_eq!(&buffer[..read], b"before-tap");

        let (sender, receiver) = async_channel::unbounded();
        tap.install(sender);

        let read = pty.reader().read(&mut buffer).expect("read succeeds");
        assert_eq!(&buffer[..read], b"after-tap");

        assert_eq!(
            receiver.try_recv().expect("tapped chunk"),
            b"after-tap".to_vec()
        );
        assert!(
            receiver.try_recv().is_err(),
            "untapped read was not recorded"
        );
    }

    #[test]
    fn tee_pty_reports_osc7_to_the_remote_cwd_tracker() {
        let remote_cwd = RemoteCwdTracker::new(PathStyle::Unix);
        let mut pty = TeePty::new(
            FakePty {
                reader: io::Cursor::new(b"\x1b]7;file://host/srv\x07$ ".to_vec()),
                writer: Vec::new(),
            },
            SessionLogTap::default(),
            Some(remote_cwd.clone()),
            None,
        );

        let mut buffer = [0u8; 64];
        let read = pty.reader().read(&mut buffer).expect("read succeeds");

        assert_eq!(read, b"\x1b]7;file://host/srv\x07$ ".len());
        assert_eq!(remote_cwd.current(), Some(PathBuf::from("/srv")));
    }

    /// Hands out the stream in fixed-size reads, the way a PTY splits output
    /// wherever its buffers happen to end, and then reports that nothing more
    /// is available for now (`Ok(0)`, as on Windows).
    struct ChunkedReader {
        stream: Vec<u8>,
        position: usize,
        chunk_size: usize,
        /// Where the reader once reports that nothing is available yet,
        /// though more is to come.
        pause_at: Option<usize>,
    }

    impl io::Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(pause_at) = self.pause_at
                && self.position >= pause_at
            {
                self.pause_at = None;
                return Ok(0);
            }
            let pause_end = self
                .pause_at
                .map_or(usize::MAX, |pause_at| pause_at.max(self.position));
            let end = (self.position + self.chunk_size.min(buf.len()))
                .min(self.stream.len())
                .min(pause_end);
            let chunk = &self.stream[self.position..end];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.position = end;
            Ok(chunk.len())
        }
    }

    struct ChunkedPty {
        reader: ChunkedReader,
        writer: Vec<u8>,
    }

    impl tty::EventedReadWrite for ChunkedPty {
        type Reader = ChunkedReader;
        type Writer = Vec<u8>;

        unsafe fn register(
            &mut self,
            _: &Arc<Poller>,
            _: PollingEvent,
            _: PollMode,
        ) -> io::Result<()> {
            Ok(())
        }

        fn reregister(&mut self, _: &Arc<Poller>, _: PollingEvent, _: PollMode) -> io::Result<()> {
            Ok(())
        }

        fn deregister(&mut self, _: &Arc<Poller>) -> io::Result<()> {
            Ok(())
        }

        fn reader(&mut self) -> &mut Self::Reader {
            &mut self.reader
        }

        fn writer(&mut self) -> &mut Self::Writer {
            &mut self.writer
        }
    }

    const NONCE: &str = "0123456789abcdef";

    fn persistent_session_stream() -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\x1b]7719;replay-begin;0123456789abcdef\x07");
        stream.extend_from_slice(b"old output\r\n\x1b]7;file://host/srv/app\x07$ ");
        stream.extend_from_slice(b"\x1b]7719;replay-end;0123456789abcdef\x07");
        stream.extend_from_slice(b"make\r\n\x1b]7719;replay-end;ffffffffffffffff\x07done\r\n");
        // Held back almost to the end before it turns out not to be a marker.
        stream.extend_from_slice(b"\x1b]7719;replay-end;0123456789abcdefX\x07 still live");
        stream
    }

    /// Reads a persistent session's stream through a [`TeePty`] the way the
    /// alacritty event loop does, returning what the emulator got and what the
    /// session log got.
    fn read_through_replay_filter(
        chunk_size: usize,
        buffer_size: usize,
    ) -> (
        Vec<u8>,
        Vec<u8>,
        RemoteCwdTracker,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        read_stream_through_replay_filter(
            persistent_session_stream(),
            None,
            chunk_size,
            buffer_size,
        )
    }

    /// Reads `stream` until the first `Ok(0)` after `pause_at` (or the
    /// first one at all without it).
    fn read_stream_through_replay_filter(
        stream: Vec<u8>,
        pause_at: Option<usize>,
        chunk_size: usize,
        buffer_size: usize,
    ) -> (
        Vec<u8>,
        Vec<u8>,
        RemoteCwdTracker,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let tap = SessionLogTap::default();
        let (sender, receiver) = async_channel::unbounded();
        tap.install(sender);
        let remote_cwd = RemoteCwdTracker::new(PathStyle::Unix);
        let attached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut pty = TeePty::new(
            ChunkedPty {
                reader: ChunkedReader {
                    stream,
                    position: 0,
                    chunk_size,
                    pause_at,
                },
                writer: Vec::new(),
            },
            tap,
            Some(remote_cwd.clone()),
            Some(ReplayMarkerFilter::new(NONCE, attached.clone())),
        );

        let mut shown = Vec::new();
        let mut buffer = vec![0u8; buffer_size];
        let mut pauses_left = usize::from(pause_at.is_some());
        loop {
            match pty.reader().read(&mut buffer) {
                Ok(0) if pauses_left > 0 => pauses_left -= 1,
                Ok(0) => break,
                Ok(read) => shown.extend_from_slice(&buffer[..read]),
                Err(error) => panic!("unexpected read error: {error}"),
            }
        }
        let mut logged = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            logged.extend(chunk);
        }
        (shown, logged, remote_cwd, attached)
    }

    #[test]
    fn tee_pty_removes_replay_markers_and_keeps_the_replay_out_of_the_log() {
        let expected_logged = b"make\r\n\x1b]7719;replay-end;ffffffffffffffff\x07done\r\n\x1b]7719;replay-end;0123456789abcdefX\x07 still live".to_vec();
        let mut expected_shown = b"old output\r\n\x1b]7;file://host/srv/app\x07$ ".to_vec();
        expected_shown.extend_from_slice(&expected_logged);
        // A marker is 37 bytes long here, so the smaller buffers leave barely
        // more room than what the filter holds back.
        for (chunk_size, buffer_size) in [(4096, 4096), (1, 4096), (3, 40), (7, 38), (1, 38)] {
            let (shown, logged, remote_cwd, attached) =
                read_through_replay_filter(chunk_size, buffer_size);
            assert_eq!(
                String::from_utf8_lossy(&shown),
                String::from_utf8_lossy(&expected_shown),
                "reads of {chunk_size} bytes into a buffer of {buffer_size}"
            );
            assert_eq!(
                String::from_utf8_lossy(&logged),
                String::from_utf8_lossy(&expected_logged),
                "reads of {chunk_size} bytes into a buffer of {buffer_size}"
            );
            // The replay tells where the shell was before the connection
            // dropped, which is exactly what a later restart needs.
            assert_eq!(remote_cwd.current(), Some(PathBuf::from("/srv/app")));
            assert!(attached.load(std::sync::atomic::Ordering::Acquire));
        }
    }

    #[test]
    fn tee_pty_keeps_the_start_of_a_marker_across_a_read_of_nothing() {
        let stream = persistent_session_stream();
        let end_marker = b"\x1b]7719;replay-end;0123456789abcdef\x07";
        let in_end_marker = stream
            .windows(end_marker.len())
            .position(|window| window == end_marker)
            .expect("the stream has an end marker")
            + 10;
        let (shown, logged, _, _) =
            read_stream_through_replay_filter(stream, Some(in_end_marker), 4096, 4096);
        assert!(
            !shown
                .windows(b"7719;replay-end;0123456789abcdef\x07".len())
                .any(|window| { window == b"7719;replay-end;0123456789abcdef\x07" }),
            "the marker still comes out whole after the pause: {:?}",
            String::from_utf8_lossy(&shown)
        );
        assert!(
            logged.starts_with(b"make\r\n"),
            "the replay stays out of the log: {:?}",
            String::from_utf8_lossy(&logged)
        );
    }

    #[test]
    fn tee_pty_drops_the_unfinished_start_of_a_marker_at_the_end() {
        let mut stream = b"\x1b]7719;replay-begin;0123456789abcdef\x07\x1b]7719;replay-end;0123456789abcdef\x07live".to_vec();
        stream.extend_from_slice(b"\x1b]7719;replay-e");
        let (shown, logged, _, _) = read_stream_through_replay_filter(stream, None, 4096, 4096);
        // An OSC that never ends shows nothing in the emulator either.
        assert_eq!(shown, b"live".to_vec());
        assert_eq!(logged, b"live".to_vec());
    }

    #[test]
    fn tee_pty_never_reports_a_chunk_of_markers_as_the_end_of_output() {
        let (shown, _, _, _) =
            read_through_replay_filter(b"\x1b]7719;replay-begin;0123456789abcdef\x07".len(), 4096);
        assert!(
            shown.starts_with(b"old output"),
            "a read that was all marker must be followed by the next one: {:?}",
            String::from_utf8_lossy(&shown)
        );
    }

    /// Behaves like alacritty's Windows PTY pipe while output arrives faster
    /// than the event loop parses it: each read finds another chunk that came
    /// in meanwhile, and a readable event is posted only on the first
    /// registration, never while data is left (the pipe posts only to a
    /// reader that found it empty).
    #[cfg(windows)]
    struct BurstPty {
        output: Vec<u8>,
        handed_out: Arc<std::sync::atomic::AtomicUsize>,
        writer: Vec<u8>,
    }

    #[cfg(windows)]
    impl io::Read for BurstPty {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            use std::sync::atomic::Ordering;

            let position = self.handed_out.load(Ordering::Acquire);
            let length = buf.len().min(16 * 1024).min(self.output.len() - position);
            buf[..length].copy_from_slice(&self.output[position..position + length]);
            self.handed_out.store(position + length, Ordering::Release);
            Ok(length)
        }
    }

    #[cfg(windows)]
    impl tty::EventedReadWrite for BurstPty {
        type Reader = Self;
        type Writer = Vec<u8>;

        unsafe fn register(
            &mut self,
            poller: &Arc<Poller>,
            interest: PollingEvent,
            _: PollMode,
        ) -> io::Result<()> {
            use polling::os::iocp::{CompletionPacket, PollerIocpExt as _};

            let mut event = interest;
            event.key = tty::PTY_READ_WRITE_TOKEN;
            poller.post(CompletionPacket::new(event))
        }

        fn reregister(&mut self, _: &Arc<Poller>, _: PollingEvent, _: PollMode) -> io::Result<()> {
            Ok(())
        }

        fn deregister(&mut self, _: &Arc<Poller>) -> io::Result<()> {
            Ok(())
        }

        fn reader(&mut self) -> &mut Self::Reader {
            self
        }

        fn writer(&mut self) -> &mut Self::Writer {
            &mut self.writer
        }
    }

    #[cfg(windows)]
    impl tty::EventedPty for BurstPty {
        fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
            None
        }
    }

    #[cfg(windows)]
    impl alacritty_terminal::event::OnResize for BurstPty {
        fn on_resize(&mut self, _: WindowSize) {}
    }

    /// The event loop stops after 64 KiB per wakeup; the rest of a burst must
    /// still be read without anything else happening in the terminal.
    #[cfg(windows)]
    #[test]
    fn tee_pty_keeps_the_event_loop_reading_a_long_burst_on_windows() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        let output = b"y\r\n".repeat(100_000);
        for replay_markers in [
            None,
            Some(ReplayMarkerFilter::new(
                NONCE,
                Arc::new(AtomicBool::new(false)),
            )),
        ] {
            let handed_out = Arc::new(AtomicUsize::new(0));
            let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
            let term = Term::new(
                pty_term_config(1000, SettingsCursorShape::default()),
                &TerminalBounds::default(),
                ZedListener(events_tx.clone()),
            );
            let pty = TeePty::new(
                BurstPty {
                    output: output.clone(),
                    handed_out: handed_out.clone(),
                    writer: Vec::new(),
                },
                SessionLogTap::default(),
                None,
                replay_markers,
            );
            let event_loop = EventLoop::new(
                Arc::new(FairMutex::new(term)),
                ZedListener(events_tx),
                pty,
                false,
                false,
            )
            .expect("the event loop starts");
            let sender = event_loop.channel();
            let event_loop = event_loop.spawn();

            let deadline = Instant::now() + Duration::from_secs(10);
            while handed_out.load(Ordering::Acquire) < output.len() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let read = handed_out.load(Ordering::Acquire);
            sender
                .send(Msg::Shutdown)
                .expect("the event loop takes the shutdown");
            event_loop.join().expect("the event loop exits");
            assert_eq!(read, output.len(), "the event loop stopped reading");
        }
    }

    #[test]
    fn terminal_hyperlink_from_alacritty_keeps_alacritty_storage() {
        let hyperlink = AlacHyperlink::new(Some("id"), "https://example.com".to_string());
        let hyperlink = terminal_hyperlink_from_alacritty(hyperlink);

        assert!(matches!(&hyperlink.data, HyperlinkData::Alacritty(_)));
        assert_eq!(hyperlink.id(), Some("id"));
        assert_eq!(hyperlink.uri(), "https://example.com");
    }

    #[test]
    fn terminal_cell_from_alacritty_shares_extra_storage() {
        let mut cell = AlacCell::default();
        cell.push_zerowidth('a');

        let converted = terminal_cell_from_alacritty(&cell);

        match (&cell.extra, &converted.cell.extra) {
            (Some(extra), Some(converted_extra)) => assert!(Arc::ptr_eq(extra, converted_extra)),
            _ => panic!("expected extra storage on both cells"),
        }
    }

    #[test]
    fn terminal_modes_round_trip_alacritty_flags() {
        let alacritty_modes = TermMode::APP_CURSOR
            | TermMode::BRACKETED_PASTE
            | TermMode::ALT_SCREEN
            | TermMode::MOUSE_DRAG
            | TermMode::SGR_MOUSE
            | TermMode::VI;

        let terminal_modes = terminal_modes_from_alacritty(alacritty_modes);
        assert!(terminal_modes.contains(Modes::APP_CURSOR));
        assert!(terminal_modes.contains(Modes::BRACKETED_PASTE));
        assert!(terminal_modes.contains(Modes::ALT_SCREEN));
        assert!(terminal_modes.contains(Modes::MOUSE_DRAG));
        assert!(terminal_modes.intersects(Modes::MOUSE_MODE));
        assert!(terminal_modes.contains(Modes::SGR_MOUSE));
        assert!(terminal_modes.contains(Modes::VI));
        assert!(!terminal_modes.contains(Modes::MOUSE_REPORT_CLICK));

        let alacritty_modes = terminal_modes.to_alacritty();
        assert!(alacritty_modes.contains(TermMode::APP_CURSOR));
        assert!(alacritty_modes.contains(TermMode::BRACKETED_PASTE));
        assert!(alacritty_modes.contains(TermMode::ALT_SCREEN));
        assert!(alacritty_modes.contains(TermMode::MOUSE_DRAG));
        assert!(alacritty_modes.contains(TermMode::SGR_MOUSE));
        assert!(alacritty_modes.contains(TermMode::VI));
        assert!(!alacritty_modes.contains(TermMode::MOUSE_REPORT_CLICK));
    }

    #[test]
    fn terminal_selection_range_round_trip_alacritty_range() {
        let alacritty_range = AlacSelectionRange {
            start: AlacPoint::new(Line(-2), Column(3)),
            end: AlacPoint::new(Line(4), Column(8)),
            is_block: true,
        };

        let terminal_range = terminal_selection_range_from_alacritty(alacritty_range);
        assert_eq!(
            terminal_range,
            SelectionRange {
                start: Point {
                    line: -2,
                    column: 3
                },
                end: Point { line: 4, column: 8 },
                is_block: true,
            }
        );
    }

    #[test]
    fn semantic_selection_stops_at_tree_branch() {
        let config = pty_term_config(1000, SettingsCursorShape::default());
        let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
        let mut term = Term::new(config, &TerminalBounds::default(), ZedListener(events_tx));
        for character in "└─zms-demo.target".chars() {
            term.input(character);
        }

        let selection = Selection::new(
            SelectionType::Semantic,
            Point::new(0, 2),
            SelectionSide::Left,
        );
        set_selection(&mut term, Some(&selection));

        assert_eq!(selection_text(&term).as_deref(), Some("zms-demo.target"));
    }
}
