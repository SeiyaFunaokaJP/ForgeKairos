//! ADR-F49: the IDE channel between a sandboxed agent CLI and Zed.
//!
//! The sandboxed program connects to `127.0.0.1:<port>` on its own loopback.
//! The sandbox helper's parent, which is outside the namespaces, accepts that
//! connection on a listener the holder created inside them and splices it to a
//! Unix socket, the *rendezvous* (see `linux_remote_helper`). The **bridge**
//! listens there: `--ide-bridge`, which Zed starts inside WSL — outside the
//! sandbox — through the same verified start as the helper. It carries every
//! rendezvous connection over its own stdin / stdout, multiplexed with the frame
//! protocol below, to `wsl.exe` and so to Zed, which takes the connections
//! apart again with [`spawn_bridge_mux`]. Nothing depends on WSL's networking
//! mode, and nothing listens on the Windows host.
//!
//! # Frames
//!
//! `[kind: u8][connection_id: u32 LE][length: u32 LE][payload]`, with `length`
//! at most [`MAX_BRIDGE_FRAME_PAYLOAD`]:
//!
//! - `1` Open (bridge → Zed, no payload): a new connection.
//! - `2` Data (either direction): bytes for that connection.
//! - `3` Close (either direction, no payload): the connection is over, in both
//!   directions. A frame naming a connection that is already closed is
//!   ignored, because either side may close while the other is still sending.
//!
//! An unknown kind, an oversized frame, a payload on Open or Close, a stream
//! that ends inside a frame, or Zed opening a connection is a protocol error,
//! and ends the whole channel: the protocol has no way to resynchronize.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::FutureExt as _;
use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable, BoxFuture, Either};
use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use futures::stream::{BoxStream, SelectAll, Stream, StreamExt as _};

/// The most payload one frame may carry.
pub const MAX_BRIDGE_FRAME_PAYLOAD: usize = 64 * 1024;

const FRAME_HEADER_LENGTH: usize = 9;

/// What a [`BridgeFrame`] says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeFrameKind {
    /// A new connection (bridge → Zed only; no payload).
    Open,
    /// Bytes for a connection (either direction).
    Data,
    /// The connection is over in both directions (either direction; no
    /// payload).
    Close,
}

impl BridgeFrameKind {
    fn code(self) -> u8 {
        match self {
            Self::Open => 1,
            Self::Data => 2,
            Self::Close => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Open),
            2 => Some(Self::Data),
            3 => Some(Self::Close),
            _ => None,
        }
    }
}

/// One frame of the bridge protocol (see the module documentation).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeFrame {
    pub kind: BridgeFrameKind,
    pub connection_id: u32,
    /// Always empty for [`BridgeFrameKind::Open`] and
    /// [`BridgeFrameKind::Close`].
    pub payload: Vec<u8>,
}

impl BridgeFrame {
    pub fn open(connection_id: u32) -> Self {
        Self {
            kind: BridgeFrameKind::Open,
            connection_id,
            payload: Vec::new(),
        }
    }

    pub fn data(connection_id: u32, payload: Vec<u8>) -> Self {
        Self {
            kind: BridgeFrameKind::Data,
            connection_id,
            payload,
        }
    }

    pub fn close(connection_id: u32) -> Self {
        Self {
            kind: BridgeFrameKind::Close,
            connection_id,
            payload: Vec::new(),
        }
    }
}

/// Append `frame` to `out` in its wire form.
///
/// A Data payload longer than [`MAX_BRIDGE_FRAME_PAYLOAD`] goes out as several
/// Data frames in order, which the receiver cannot tell apart from one. Open
/// and Close go out without a payload, whatever `payload` holds.
pub fn encode_frame(frame: &BridgeFrame, out: &mut Vec<u8>) {
    match frame.kind {
        BridgeFrameKind::Data if frame.payload.len() > MAX_BRIDGE_FRAME_PAYLOAD => {
            for chunk in frame.payload.chunks(MAX_BRIDGE_FRAME_PAYLOAD) {
                encode_header(out, frame.kind, frame.connection_id, chunk.len());
                out.extend_from_slice(chunk);
            }
        }
        BridgeFrameKind::Data => {
            encode_header(out, frame.kind, frame.connection_id, frame.payload.len());
            out.extend_from_slice(&frame.payload);
        }
        BridgeFrameKind::Open | BridgeFrameKind::Close => {
            encode_header(out, frame.kind, frame.connection_id, 0)
        }
    }
}

fn encode_header(out: &mut Vec<u8>, kind: BridgeFrameKind, connection_id: u32, length: usize) {
    out.push(kind.code());
    out.extend_from_slice(&connection_id.to_le_bytes());
    // Never more than `MAX_BRIDGE_FRAME_PAYLOAD` here; if it ever were, the
    // receiver refuses the frame instead of reading a truncated length.
    let length = u32::try_from(length).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_le_bytes());
}

/// Reassembles [`BridgeFrame`]s from a byte stream that arrives in pieces of
/// any size.
#[derive(Debug, Default)]
pub struct BridgeFrameDecoder {
    buffer: Vec<u8>,
}

impl BridgeFrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next bytes of the stream and take every frame they complete.
    ///
    /// A frame's header is checked as soon as it is complete, so an oversized
    /// frame is refused before its payload is buffered. After an error the
    /// stream is unusable.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<BridgeFrame>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut consumed = 0;
        while let Some(rest) = self.buffer.get(consumed..) {
            let Some(header) = rest.get(..FRAME_HEADER_LENGTH) else {
                break;
            };
            let (kind, connection_id, length) = parse_header(header)?;
            let Some(payload) = rest.get(FRAME_HEADER_LENGTH..FRAME_HEADER_LENGTH + length) else {
                break;
            };
            frames.push(BridgeFrame {
                kind,
                connection_id,
                payload: payload.to_vec(),
            });
            consumed += FRAME_HEADER_LENGTH + length;
        }
        self.buffer.drain(..consumed);
        Ok(frames)
    }

    /// Whether the stream may end here, which is only between frames.
    pub fn finish(&self) -> Result<()> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            bail!(
                "the IDE bridge stream ended inside a frame ({} bytes left over)",
                self.buffer.len()
            )
        }
    }
}

fn parse_header(header: &[u8]) -> Result<(BridgeFrameKind, u32, usize)> {
    let [code, id0, id1, id2, id3, length0, length1, length2, length3] = header else {
        bail!("bug: an IDE bridge frame header is {FRAME_HEADER_LENGTH} bytes");
    };
    let kind = BridgeFrameKind::from_code(*code)
        .with_context(|| format!("unknown IDE bridge frame kind {code}"))?;
    let connection_id = u32::from_le_bytes([*id0, *id1, *id2, *id3]);
    let length = u32::from_le_bytes([*length0, *length1, *length2, *length3]);
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if length > MAX_BRIDGE_FRAME_PAYLOAD {
        bail!(
            "IDE bridge frame for connection {connection_id} is {length} bytes, \
             more than the {MAX_BRIDGE_FRAME_PAYLOAD} a frame may carry"
        );
    }
    if kind != BridgeFrameKind::Data && length != 0 {
        bail!("IDE bridge {kind:?} frame for connection {connection_id} carries a payload");
    }
    Ok((kind, connection_id, length))
}

/// The most lock-file content the bridge accepts as its stdin preamble.
pub const MAX_IDE_LOCK_CONTENT: usize = 64 * 1024;

/// Encode the IDE lock-file content as the bridge's stdin preamble: a
/// little-endian `u32` length followed by the bytes (ADR-F49 item 5).
///
/// The content carries the CLI's IDE auth token, so it travels on stdin rather
/// than on the bridge's command line, where `/proc/<pid>/cmdline` would expose
/// it. The caller writes this to the bridge's stdin before any frame; the
/// bridge reads exactly this, writes the lock file, then switches the same
/// stdin to the frame protocol.
pub fn encode_ide_lock_preamble(content: &[u8]) -> Vec<u8> {
    let length = u32::try_from(content.len()).unwrap_or(u32::MAX);
    let mut preamble = Vec::with_capacity(4 + content.len());
    preamble.extend_from_slice(&length.to_le_bytes());
    preamble.extend_from_slice(content);
    preamble
}

/// ADR-F49 (new decision): the most IDE bridges one Zed keeps running at once.
///
/// A bridge is not free: a `wsl.exe` of this process, a helper process inside
/// the distro, a rendezvous socket and a multiplexer. One is started for every
/// sandboxed agent connection (決定 35 took the exclusive claim off that path,
/// so one project and one agent no longer cap it at a single bridge) and for
/// every sandboxed CLI session, so their number follows how many windows,
/// projects and agents are open and nothing bounded it. Roomy enough that no
/// ordinary session meets it, and the idle disconnect gives back the ones
/// nobody uses.
pub const MAX_IDE_BRIDGES: usize = 16;

static RUNNING_IDE_BRIDGES: AtomicUsize = AtomicUsize::new(0);

/// One of [`MAX_IDE_BRIDGES`]. Hold it for as long as its bridge runs — a
/// dropped slot is a free one, and nothing else counts bridges.
///
/// A bridge over the limit is **refused**, never traded for an older one. What
/// a bridge carries is opened from inside the sandbox, so an agent that asks
/// for bridges is the untrusted side of this, and closing the oldest would let
/// it take the IDE channel away from every session that was already working:
/// the running agent would lose its diagnostics tool, and the CLI's lock file
/// goes with its bridge. Refusing costs the *new* session that tool alone,
/// which is the case its caller already handles — it starts the agent without
/// an IDE server and says so in the log.
pub struct IdeBridgeSlot;

impl IdeBridgeSlot {
    /// Takes a slot, or `None` when [`MAX_IDE_BRIDGES`] are already running.
    pub fn take() -> Option<Self> {
        RUNNING_IDE_BRIDGES
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |running| {
                (running < MAX_IDE_BRIDGES).then(|| running + 1)
            })
            .ok()
            .map(|_| Self)
    }

    #[cfg(test)]
    fn running() -> usize {
        RUNNING_IDE_BRIDGES.load(Ordering::SeqCst)
    }
}

impl Drop for IdeBridgeSlot {
    fn drop(&mut self) {
        RUNNING_IDE_BRIDGES.fetch_sub(1, Ordering::SeqCst);
    }
}

// ===========================================================================
// Zed's side: demultiplexing the bridge's standard I/O
// ===========================================================================

/// How many bytes may wait for one connection's reader. When a frame would
/// take a connection past this because Zed isn't reading it, the mux closes
/// *that* connection (dropping its data and telling the bridge) rather than
/// stalling the shared pipe or buffering without limit, so one slow reader
/// can't hold up the others. Counted in bytes, not frames, so a burst of small
/// frames is no reason to close anything.
const INBOUND_BYTES_PER_CONNECTION: usize = 1024 * 1024;
/// How many chunks one connection's writes may queue before its writer waits.
const OUTBOUND_CHUNKS_PER_CONNECTION: usize = 16;
const READ_BUFFER_SIZE: usize = 64 * 1024;
/// The most IDE connections the mux hands out at once. New ones beyond this —
/// while the consumer has not taken the queued ones — are refused and closed
/// back to the bridge, which caps the same count on its side.
pub const MAX_BRIDGE_CONNECTIONS: usize = 64;

#[derive(Default)]
struct ConnectionState {
    /// A Close frame arrived from the bridge: writes fail, and no Close goes
    /// back.
    closed_by_bridge: AtomicBool,
    /// The mux ended while the connection was still open.
    aborted: AtomicBool,
    /// Bytes queued for the reader and not yet taken by it.
    inbound_bytes: AtomicUsize,
}

/// One connection the sandboxed program opened to its IDE port, as Zed sees
/// it.
///
/// The protocol closes both directions at once: closing (or dropping) this
/// ends reading too, once what already arrived has been read, and a Close
/// from the bridge makes reads end and writes fail. When the whole channel
/// goes away without a Close, reads fail with
/// [`io::ErrorKind::ConnectionAborted`] instead of ending cleanly.
pub struct BridgeConnection {
    connection_id: u32,
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    pending: Vec<u8>,
    pending_offset: usize,
    outbound: mpsc::Sender<Vec<u8>>,
    state: Arc<ConnectionState>,
}

impl BridgeConnection {
    /// The id the bridge gave this connection.
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }
}

impl AsyncRead for BridgeConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            if let Some(available) = this
                .pending
                .get(this.pending_offset..)
                .filter(|available| !available.is_empty())
            {
                let count = available.len().min(buf.len());
                if let (Some(target), Some(source)) = (buf.get_mut(..count), available.get(..count))
                {
                    target.copy_from_slice(source);
                }
                this.pending_offset += count;
                return Poll::Ready(Ok(count));
            }
            match this.inbound.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(chunk)) => {
                    this.state
                        .inbound_bytes
                        .fetch_sub(chunk.len(), Ordering::SeqCst);
                    this.pending = chunk;
                    this.pending_offset = 0;
                }
                Poll::Ready(None) if this.state.aborted.load(Ordering::SeqCst) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "the IDE bridge channel stopped",
                    )));
                }
                Poll::Ready(None) => return Poll::Ready(Ok(0)),
            }
        }
    }
}

impl AsyncWrite for BridgeConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.state.closed_by_bridge.load(Ordering::SeqCst)
            || this.state.aborted.load(Ordering::SeqCst)
        {
            return Poll::Ready(Err(connection_closed()));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        match this.outbound.poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_)) => return Poll::Ready(Err(connection_closed())),
            Poll::Ready(Ok(())) => {}
        }
        let chunk = buf.get(..MAX_BRIDGE_FRAME_PAYLOAD).unwrap_or(buf);
        match this.outbound.start_send(chunk.to_vec()) {
            Ok(()) => Poll::Ready(Ok(chunk.len())),
            Err(_) => Poll::Ready(Err(connection_closed())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().outbound.close_channel();
        Poll::Ready(Ok(()))
    }
}

fn connection_closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the IDE bridge connection is closed",
    )
}

/// The connections the bridge opens, in order. Ends when the channel does.
pub struct BridgeConnections {
    connections: mpsc::Receiver<BridgeConnection>,
}

impl Stream for BridgeConnections {
    type Item = BridgeConnection;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.connections.poll_next_unpin(cx)
    }
}

/// Keeps the mux running; dropping it shuts the mux down, which ends
/// [`BridgeConnections`] and every connection still open.
pub struct BridgeMuxHandle {
    abort: AbortHandle,
}

impl Drop for BridgeMuxHandle {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

type ConnectionTable = Arc<Mutex<HashMap<u32, InboundConnection>>>;

/// The mux's end of a connection's reading half.
struct InboundConnection {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    state: Arc<ConnectionState>,
}

/// A new connection's writing half, handed from the reading loop to the
/// writing loop.
struct Registration {
    connection_id: u32,
    outbound: mpsc::Receiver<Vec<u8>>,
    state: Arc<ConnectionState>,
}

fn lock_table(
    table: &Mutex<HashMap<u32, InboundConnection>>,
) -> MutexGuard<'_, HashMap<u32, InboundConnection>> {
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Marks every connection still open as aborted when the mux ends, however it
/// ends — including when its future is simply dropped.
struct EndOfMux {
    table: ConnectionTable,
}

impl Drop for EndOfMux {
    fn drop(&mut self) {
        let open = std::mem::take(&mut *lock_table(&self.table));
        for connection in open.into_values() {
            // Set before its sender is dropped, so a reader that sees the
            // channel end also sees why.
            connection.state.aborted.store(true, Ordering::SeqCst);
        }
    }
}

/// Take apart the bridge's multiplexed stream: `reader` is the bridge's
/// stdout, `writer` its stdin.
///
/// The mux runs as one future handed to `spawn` (for example
/// `|future| cx.background_spawn(future).detach()`), reading and writing
/// concurrently so that neither direction can hold up the other. It ends when
/// the bridge's output ends, on a protocol or I/O error (logged), or when the
/// returned handle is dropped.
pub fn spawn_bridge_mux<R, W>(
    reader: R,
    writer: W,
    spawn: impl FnOnce(BoxFuture<'static, ()>),
) -> (BridgeConnections, BridgeMuxHandle)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Bounded: the queue of connections the consumer has not taken yet caps at
    // `MAX_BRIDGE_CONNECTIONS`; one beyond that is refused and closed back to
    // the bridge (see `open_connection`).
    let (connections_sender, connections) = mpsc::channel(MAX_BRIDGE_CONNECTIONS);
    let (registrations_sender, registrations) = mpsc::unbounded();
    let (force_close_sender, force_close) = mpsc::unbounded();
    let table = ConnectionTable::default();
    let (abort, abort_registration) = AbortHandle::new_pair();
    let mux = async move {
        let _end = EndOfMux {
            table: table.clone(),
        };
        let reading = read_from_bridge(
            reader,
            table.clone(),
            registrations_sender,
            connections_sender,
            force_close_sender,
        )
        .boxed();
        let writing = write_to_bridge(writer, table, registrations, force_close).boxed();
        let outcome = match futures::future::select(reading, writing).await {
            Either::Left((outcome, _)) | Either::Right((outcome, _)) => outcome,
        };
        if let Err(error) = outcome {
            log::warn!("the IDE bridge channel stopped: {error:#}");
        }
    };
    spawn(Abortable::new(mux, abort_registration).map(drop).boxed());
    (BridgeConnections { connections }, BridgeMuxHandle { abort })
}

async fn read_from_bridge<R: AsyncRead + Unpin>(
    mut reader: R,
    table: ConnectionTable,
    registrations: mpsc::UnboundedSender<Registration>,
    mut connections: mpsc::Sender<BridgeConnection>,
    force_close: mpsc::UnboundedSender<u32>,
) -> Result<()> {
    let mut decoder = BridgeFrameDecoder::new();
    let mut buffer = vec![0; READ_BUFFER_SIZE];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("reading from the IDE bridge")?;
        if read == 0 {
            return decoder.finish();
        }
        let bytes = buffer.get(..read).context("bug: read past the buffer")?;
        for frame in decoder.push(bytes)? {
            match frame.kind {
                BridgeFrameKind::Open => open_connection(
                    frame.connection_id,
                    &table,
                    &registrations,
                    &mut connections,
                )?,
                BridgeFrameKind::Data => {
                    // Never blocks the shared pipe: a connection whose inbound
                    // buffer is full (Zed is not reading it) is closed, rather
                    // than holding up every other connection's frames.
                    if let Delivery::Full = deliver(frame.connection_id, frame.payload, &table) {
                        if let Some(connection) = lock_table(&table).remove(&frame.connection_id) {
                            connection.state.aborted.store(true, Ordering::SeqCst);
                        }
                        force_close.unbounded_send(frame.connection_id).ok();
                    }
                }
                BridgeFrameKind::Close => {
                    let closed = lock_table(&table).remove(&frame.connection_id);
                    if let Some(connection) = closed {
                        connection
                            .state
                            .closed_by_bridge
                            .store(true, Ordering::SeqCst);
                    }
                }
            }
        }
    }
}

fn open_connection(
    connection_id: u32,
    table: &ConnectionTable,
    registrations: &mpsc::UnboundedSender<Registration>,
    connections: &mut mpsc::Sender<BridgeConnection>,
) -> Result<()> {
    let state = Arc::new(ConnectionState::default());
    // Unbounded as a channel, bounded in bytes by `deliver`.
    let (inbound_sender, inbound) = mpsc::unbounded();
    let (outbound, outbound_receiver) = mpsc::channel(OUTBOUND_CHUNKS_PER_CONNECTION);
    {
        let mut table = lock_table(table);
        if table.contains_key(&connection_id) {
            bail!("the IDE bridge opened connection {connection_id} twice");
        }
        table.insert(
            connection_id,
            InboundConnection {
                sender: inbound_sender,
                state: state.clone(),
            },
        );
    }
    registrations
        .unbounded_send(Registration {
            connection_id,
            outbound: outbound_receiver,
            state: state.clone(),
        })
        .map_err(|_| anyhow!("the IDE bridge writer stopped"))?;
    let connection = BridgeConnection {
        connection_id,
        inbound,
        pending: Vec::new(),
        pending_offset: 0,
        outbound,
        state,
    };
    // Refused when the consumer is behind by `MAX_BRIDGE_CONNECTIONS` or has
    // gone away: dropping the connection ends its outbound half, so the writer
    // sends a Close back to the bridge (which frees its own slot).
    if let Err(error) = connections.try_send(connection) {
        log::debug!(
            "refusing IDE bridge connection {connection_id}: {}",
            if error.is_full() {
                "the connection queue is full"
            } else {
                "nobody accepts connections"
            }
        );
    }
    Ok(())
}

/// The outcome of handing a Data payload to a connection's reader.
enum Delivery {
    /// Queued (or the connection is gone, which drops it): nothing more to do.
    DoneOrGone,
    /// The reader's buffer is full — the caller closes the connection.
    Full,
}

/// Hand `payload` to the connection's reader without blocking. A payload that
/// would take the connection past [`INBOUND_BYTES_PER_CONNECTION`] is reported
/// so the caller can close that one connection instead of stalling the shared
/// pipe.
fn deliver(connection_id: u32, payload: Vec<u8>, table: &ConnectionTable) -> Delivery {
    let mut table = lock_table(table);
    let Some(connection) = table.get_mut(&connection_id) else {
        return Delivery::DoneOrGone;
    };
    let length = payload.len();
    // Only this loop adds, so the total cannot pass the budget between the
    // check and the addition; the reader only takes away.
    let queued = connection.state.inbound_bytes.load(Ordering::SeqCst);
    if queued.saturating_add(length) > INBOUND_BYTES_PER_CONNECTION {
        return Delivery::Full;
    }
    connection
        .state
        .inbound_bytes
        .fetch_add(length, Ordering::SeqCst);
    // A failure means the receiver was dropped; its writing half reports the
    // close.
    if connection.sender.unbounded_send(payload).is_err() {
        log::debug!("IDE bridge connection {connection_id} stopped reading");
    }
    Delivery::DoneOrGone
}

enum Outgoing {
    Registered(Registration),
    Data(u32, Vec<u8>),
    /// The connection's writing half ended: closed or dropped on this side.
    Finished(u32),
    /// The reader loop closed the connection (its inbound buffer overflowed);
    /// tell the bridge.
    ForceClose(u32),
}

async fn write_to_bridge<W: AsyncWrite + Unpin>(
    mut writer: W,
    table: ConnectionTable,
    registrations: mpsc::UnboundedReceiver<Registration>,
    force_close: mpsc::UnboundedReceiver<u32>,
) -> Result<()> {
    let mut outgoing: SelectAll<BoxStream<'static, Outgoing>> = SelectAll::new();
    outgoing.push(registrations.map(Outgoing::Registered).boxed());
    outgoing.push(force_close.map(Outgoing::ForceClose).boxed());
    let mut states: HashMap<u32, Arc<ConnectionState>> = HashMap::new();
    let mut encoded = Vec::new();
    while let Some(event) = outgoing.next().await {
        encoded.clear();
        match event {
            Outgoing::Registered(registration) => {
                let connection_id = registration.connection_id;
                states.insert(connection_id, registration.state);
                outgoing.push(
                    registration
                        .outbound
                        .map(move |chunk| Outgoing::Data(connection_id, chunk))
                        .chain(futures::stream::once(futures::future::ready(
                            Outgoing::Finished(connection_id),
                        )))
                        .boxed(),
                );
                continue;
            }
            Outgoing::Data(connection_id, chunk) => {
                let closed = states
                    .get(&connection_id)
                    .is_none_or(|state| state.closed_by_bridge.load(Ordering::SeqCst));
                if closed {
                    continue;
                }
                encode_frame(&BridgeFrame::data(connection_id, chunk), &mut encoded);
            }
            Outgoing::Finished(connection_id) => {
                let state = states.remove(&connection_id);
                // Reading ends with writing: the protocol has no half-close.
                lock_table(&table).remove(&connection_id);
                if state.is_none_or(|state| state.closed_by_bridge.load(Ordering::SeqCst)) {
                    continue;
                }
                encode_frame(&BridgeFrame::close(connection_id), &mut encoded);
            }
            Outgoing::ForceClose(connection_id) => {
                // The reader loop already removed it from the table and aborted
                // it; send the Close once (a later `Finished` finds no state and
                // stays silent).
                if states.remove(&connection_id).is_none() {
                    continue;
                }
                encode_frame(&BridgeFrame::close(connection_id), &mut encoded);
            }
        }
        writer
            .write_all(&encoded)
            .await
            .context("writing to the IDE bridge")?;
        writer.flush().await.context("writing to the IDE bridge")?;
    }
    Ok(())
}

// ===========================================================================
// The bridge process inside WSL (`--ide-bridge`)
// ===========================================================================

/// Opens the lines the bridge writes on stderr to say how its setup went:
/// `zed-ide-bridge: ready`, or `zed-ide-bridge: <code>: <detail>` before it
/// exits. Read by whoever started it (`windows_wsl::start_ide_bridge`), which
/// runs on Windows while the bridge runs in the distro, so both sides name it
/// here rather than each spelling it out. Neither side exists on macOS, where
/// this module is compiled for its frame protocol alone.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) const BRIDGE_STATUS_MARKER: &str = "zed-ide-bridge";

/// What the bridge reports when another bridge — this Zed's or another's —
/// already holds the exclusive directory it was given.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) const SESSION_RUNNING_CODE: &str = "session-running";

/// A line of the bridge's stderr that says how its setup went, or `None` for
/// any other line (its own logging). Read on the side that started the bridge,
/// which is Windows.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BridgeStatus<'a> {
    Ready,
    Failed { code: &'a str },
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn parse_bridge_status(line: &str) -> Option<BridgeStatus<'_>> {
    let rest = line
        .trim_end()
        .strip_prefix(BRIDGE_STATUS_MARKER)?
        .strip_prefix(": ")?;
    if rest == "ready" {
        return Some(BridgeStatus::Ready);
    }
    let (code, _detail) = rest.split_once(':')?;
    let code = code.trim();
    (!code.is_empty() && !code.contains(char::is_whitespace))
        .then_some(BridgeStatus::Failed { code })
}

/// The Japanese reason for a bridge setup failure `code`.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn bridge_failure_message(code: &str) -> String {
    let reason = match code {
        "bad-args" => "IDE 連携の中継の起動引数が不正です (内部エラー)。",
        "signals-failed" => "IDE 連携の中継のシグナル処理を設定できませんでした。",
        "home-unknown" => {
            "WSL のホームディレクトリを特定できないため、IDE 連携の中継を起動できませんでした。"
        }
        "rendezvous-failed" => {
            "IDE 連携の受け渡し用のソケットを作成できませんでした \
             (WSL の ~/.cache/zed-jp-enhance/ide の所有者と権限を確認してください)。"
        }
        "lock-preamble-failed" => "IDE 連携の中継に接続情報を渡せませんでした。",
        "lock-file-failed" => {
            "IDE 連携のロックファイルを作成できませんでした (WSL の ~/.zed-jp-enhance-claude の\
             所有者と権限、シンボリックリンクになっていないかを確認してください)。"
        }
        other => {
            return format!("IDE 連携の中継を起動できませんでした (不明な理由: {other})。");
        }
    };
    reason.to_string()
}

#[cfg(target_os = "linux")]
pub(crate) use linux::run_ide_bridge;

#[cfg(target_os = "linux")]
mod linux {
    use super::{BridgeFrame, BridgeFrameDecoder, BridgeFrameKind, MAX_BRIDGE_FRAME_PAYLOAD};
    use anyhow::{Context as _, Result, bail, ensure};
    use std::collections::HashMap;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Component, Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    /// Opens the bridge's lines on stderr: `ready`, or a setup failure.
    const BRIDGE_MARKER: &str = super::BRIDGE_STATUS_MARKER;
    const SETUP_FAILED_EXIT_CODE: i32 = 126;
    /// Longest file name accepted for the socket and the lock file: the
    /// socket is bound through `/proc/self/fd/<n>/<name>`, which has to fit
    /// in `sun_path`.
    const MAX_ENTRY_NAME_LENGTH: usize = 64;
    const MAX_LOCK_CONTENT_LENGTH: usize = super::MAX_IDE_LOCK_CONTENT;
    /// The most IDE connections served at once (the Zed side caps the same
    /// number). A connection accepted beyond this is closed at once.
    const MAX_IDE_CONNECTIONS: usize = super::MAX_BRIDGE_CONNECTIONS;
    /// How many bytes from Zed may wait for one connection's writer. A frame
    /// that would take it past this because the sandboxed reader stalled closes
    /// that connection, never the shared stdin pipe. Counted in bytes, not
    /// frames, so a burst of small frames is no reason to close anything.
    const WRITER_BYTES_PER_CONNECTION: usize = 1024 * 1024;
    /// How long one connection's writer waits for the sandboxed program to
    /// take what Zed sent before that connection is given up on. Set on every
    /// accepted socket, so no program can hold a writer thread — and one of
    /// [`MAX_IDE_CONNECTIONS`] — by opening a connection and never reading it.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

    /// `--ide-bridge --rendezvous <socket> [--lock-file <path>]
    /// [--exclusive-directory <directory>]`.
    ///
    /// Claims the exclusive directory first (see [`claim_exclusive_directory`];
    /// `session-running` when another bridge holds it), listens on the
    /// rendezvous socket (in a private directory of this account's), writes the
    /// IDE lock file (its content arrives as a stdin preamble, not on the
    /// command line — see [`super::encode_ide_lock_preamble`]), and multiplexes
    /// every connection
    /// over stdin / stdout until stdin ends (Zed went away), a protocol error,
    /// or SIGTERM / SIGHUP / SIGINT — then removes the socket and the lock
    /// file and exits. Never returns.
    pub(crate) fn run_ide_bridge(args: &[OsString]) -> ! {
        crate::linux_remote_helper::init_helper_logging();
        let options = match parse_bridge_args(args) {
            Ok(options) => options,
            Err(error) => fail_setup("bad-args", &error),
        };
        // Everything the bridge creates is this account's alone.
        // SAFETY: `umask` only changes this process's file-creation mask.
        unsafe { libc::umask(0o077) };
        let termination_signals = match block_termination_signals() {
            Ok(signals) => signals,
            Err(error) => fail_setup("signals-failed", &error),
        };
        // The trusted base for the no-symlink walk to the socket and the lock
        // file: the account's home, which both are under.
        let base = match crate::protected_paths::home_dir_from_account() {
            Some(home) => home,
            None => fail_setup(
                "home-unknown",
                &anyhow::anyhow!("cannot determine the account's home directory"),
            ),
        };
        // Before anything is created, so that a bridge turned away leaves
        // nothing behind. Held until the process exits, however it ends.
        let _exclusive = match options.exclusive_directory.as_ref() {
            Some(path) => match claim_exclusive_directory(path, &base) {
                Ok(Claim::Held(directory)) => Some(directory),
                Ok(Claim::Taken) => fail_setup(
                    super::SESSION_RUNNING_CODE,
                    &anyhow::anyhow!("another IDE bridge serves {}", path.display()),
                ),
                Err(error) => fail_setup("lock-file-failed", &error),
            },
            None => None,
        };
        let (rendezvous, listener) = match Rendezvous::create(&options.rendezvous, &base) {
            Ok(created) => created,
            Err(error) => fail_setup("rendezvous-failed", &error),
        };
        let lock_file = match options.lock_file.as_ref() {
            // The content is read from stdin *before* the frame protocol, so it
            // never appears on the command line.
            Some(path) => {
                let content = match read_lock_preamble() {
                    Ok(content) => content,
                    Err(error) => {
                        rendezvous.remove();
                        fail_setup("lock-preamble-failed", &error);
                    }
                };
                match LockFile::create(path, &content, &base) {
                    Ok(lock_file) => Some(lock_file),
                    Err(error) => {
                        rendezvous.remove();
                        fail_setup("lock-file-failed", &error);
                    }
                }
            }
            None => None,
        };
        *lock_cleanup() = Some(Cleanup {
            rendezvous,
            lock_file,
        });
        if let Err(error) = std::thread::Builder::new()
            .name("zed-ide-bridge-signals".into())
            .spawn(move || finish_on_termination_signal(termination_signals))
        {
            log::warn!("[ide-bridge] cleaning up on a termination signal is unavailable: {error}");
        }
        eprintln!("{BRIDGE_MARKER}: ready");
        let code = match serve_bridge(listener, io::stdin(), io::stdout()) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{BRIDGE_MARKER}: stopped: {error:#}");
                1
            }
        };
        finish(code)
    }

    #[derive(Debug)]
    struct BridgeOptions {
        rendezvous: PathBuf,
        /// When present, the lock file's path; its content comes over stdin.
        lock_file: Option<PathBuf>,
        /// When present, the directory only one bridge at a time may serve.
        exclusive_directory: Option<PathBuf>,
    }

    fn parse_bridge_args(args: &[OsString]) -> Result<BridgeOptions> {
        let mut rendezvous = None;
        let mut lock_file = None;
        let mut exclusive_directory = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.to_str().unwrap_or_default() {
                "--rendezvous" => {
                    let value = args.next().context("--rendezvous requires a path")?;
                    rendezvous = Some(PathBuf::from(value));
                }
                "--lock-file" => {
                    let value = args.next().context("--lock-file requires a path")?;
                    lock_file = Some(PathBuf::from(value));
                }
                "--exclusive-directory" => {
                    let value = args
                        .next()
                        .context("--exclusive-directory requires a path")?;
                    exclusive_directory = Some(PathBuf::from(value));
                }
                other => bail!("unknown IDE bridge flag: {other}"),
            }
        }
        Ok(BridgeOptions {
            rendezvous: rendezvous.context("--rendezvous is required")?,
            lock_file,
            exclusive_directory,
        })
    }

    /// What a claim on an exclusive directory came to.
    enum Claim {
        /// Ours until the descriptor closes.
        Held(OwnedFd),
        /// Another bridge holds it.
        Taken,
    }

    /// Open the directory `path`, walking to it as [`open_directory_tree`]
    /// does and creating it if missing, and take an exclusive `flock` on it
    /// for as long as the returned descriptor stays open: one bridge at a time
    /// serves it, whichever Zed started the bridge (ADR-F49: one sandboxed
    /// Claude Code session per configuration). The kernel lets the lock go
    /// with the process, so a bridge that crashed holds nothing.
    fn claim_exclusive_directory(path: &Path, base: &Path) -> Result<Claim> {
        let directory =
            open_directory_tree(path, base, DirectoryAccess::NotWritableByOthers)?.directory;
        // SAFETY: `directory` is an open descriptor of ours.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Claim::Held(directory));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(Claim::Taken);
        }
        Err(error).with_context(|| format!("locking {}", path.display()))
    }

    /// Read the lock-file content the caller wrote to stdin ahead of the frame
    /// protocol: a little-endian `u32` length then the bytes (see
    /// [`super::encode_ide_lock_preamble`]). The same stdin then carries frames.
    fn read_lock_preamble() -> Result<Vec<u8>> {
        let mut length_bytes = [0u8; 4];
        read_exact_fd(libc::STDIN_FILENO, &mut length_bytes)
            .context("reading the IDE lock preamble length")?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        ensure!(
            length <= MAX_LOCK_CONTENT_LENGTH,
            "the IDE lock preamble is {length} bytes, more than {MAX_LOCK_CONTENT_LENGTH}"
        );
        let mut content = vec![0u8; length];
        read_exact_fd(libc::STDIN_FILENO, &mut content).context("reading the IDE lock preamble")?;
        Ok(content)
    }

    /// Read exactly `buffer.len()` bytes from a raw fd, before anything buffers
    /// it (the preamble is taken straight off fd 0 so `serve_bridge`'s buffered
    /// stdin, which starts empty, then reads the frames that follow).
    fn read_exact_fd(fd: libc::c_int, buffer: &mut [u8]) -> Result<()> {
        let mut read = 0;
        while read < buffer.len() {
            let remaining = &mut buffer[read..];
            // SAFETY: `fd` is valid and the slice is a valid writable region of
            // its own length.
            let count = unsafe {
                libc::read(
                    fd,
                    remaining.as_mut_ptr().cast::<libc::c_void>(),
                    remaining.len(),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if count == 0 {
                bail!("stdin ended before the preamble was complete");
            }
            read += count as usize;
        }
        Ok(())
    }

    fn fail_setup(code: &str, error: &anyhow::Error) -> ! {
        eprintln!("{BRIDGE_MARKER}: {code}: {error:#}");
        std::process::exit(SETUP_FAILED_EXIT_CODE);
    }

    /// What has to go when the bridge exits.
    struct Cleanup {
        rendezvous: Rendezvous,
        lock_file: Option<LockFile>,
    }

    static CLEANUP: Mutex<Option<Cleanup>> = Mutex::new(None);

    fn lock_cleanup() -> MutexGuard<'static, Option<Cleanup>> {
        CLEANUP
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remove the socket and the lock file, then exit.
    fn finish(code: i32) -> ! {
        let mut cleanup = lock_cleanup();
        if let Some(cleanup) = cleanup.take() {
            cleanup.rendezvous.remove();
            if let Some(lock_file) = &cleanup.lock_file {
                lock_file.remove();
            }
        }
        // Still holding the slot: a second caller waits here until the
        // process is gone, rather than exiting halfway through the cleanup.
        std::process::exit(code)
    }

    /// Block the termination signals in this thread, and so in every thread
    /// it starts, leaving them to [`finish_on_termination_signal`].
    fn block_termination_signals() -> Result<libc::sigset_t> {
        // SAFETY: `signals` is initialized by `sigemptyset` before anything
        // reads it, and `pthread_sigmask` only reads it.
        unsafe {
            let mut signals: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signals);
            for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
                libc::sigaddset(&mut signals, signal);
            }
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
            if result != 0 {
                bail!("pthread_sigmask: {}", io::Error::from_raw_os_error(result));
            }
            Ok(signals)
        }
    }

    fn finish_on_termination_signal(signals: libc::sigset_t) {
        loop {
            let mut signal = 0;
            // SAFETY: `signals` was built by `block_termination_signals`, and
            // `signal` is a valid out-parameter.
            if unsafe { libc::sigwait(&signals, &mut signal) } == 0 {
                finish(128 + signal);
            }
        }
    }

    type SharedOutput = Arc<Mutex<Box<dyn Write + Send>>>;
    type Connections = Arc<Mutex<HashMap<u32, Connection>>>;

    /// One accepted connection: the socket (for shutdown) and the queue to its
    /// writer thread. Zed's Data frames go onto the queue without blocking the
    /// shared stdin; one that would take it past
    /// [`WRITER_BYTES_PER_CONNECTION`] (the sandboxed reader stalled) closes
    /// this one connection.
    struct Connection {
        stream: Arc<UnixStream>,
        writer: std::sync::mpsc::Sender<Vec<u8>>,
        /// Bytes on the queue or being written, not yet on the socket.
        queued_bytes: Arc<AtomicUsize>,
    }

    fn lock_connections(
        connections: &Mutex<HashMap<u32, Connection>>,
    ) -> MutexGuard<'_, HashMap<u32, Connection>> {
        connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Multiplex every connection accepted on `listener` over `input` /
    /// `output` until `input` ends (`Ok`) or breaks the protocol.
    ///
    /// Each connection has a writer thread with a bounded queue, so a Data
    /// frame from Zed never blocks the shared stdin on one slow reader: a full
    /// queue closes that connection instead. No more than
    /// [`MAX_IDE_CONNECTIONS`] are served at once.
    pub(super) fn serve_bridge(
        listener: UnixListener,
        input: impl Read,
        output: impl Write + Send + 'static,
    ) -> Result<()> {
        let output: SharedOutput = Arc::new(Mutex::new(Box::new(output)));
        let connections = Connections::default();
        let accepting = listener
            .try_clone()
            .context("duplicating the rendezvous listener")?;
        let accept_thread = std::thread::Builder::new()
            .name("zed-ide-bridge-accept".into())
            .spawn({
                let output = output.clone();
                let connections = connections.clone();
                move || accept_connections(accepting, &connections, &output)
            })
            .context("starting the rendezvous listener")?;

        let outcome = forward_from_zed(input, &connections, &output);

        // Stop accepting and end every connection, so the threads serving
        // them finish too.
        // SAFETY: `shutdown` of our own listening socket; it wakes the
        // blocked `accept` with `EINVAL`.
        if unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) } != 0 {
            log::debug!(
                "[ide-bridge] shutting the listener down: {}",
                io::Error::last_os_error()
            );
        }
        let open: Vec<Arc<UnixStream>> = lock_connections(&connections)
            .drain()
            .map(|(_, connection)| connection.stream)
            .collect();
        for stream in open {
            if let Err(error) = stream.shutdown(Shutdown::Both) {
                log::debug!("[ide-bridge] ending a connection: {error}");
            }
        }
        if accept_thread.join().is_err() {
            log::warn!("[ide-bridge] the rendezvous listener thread panicked");
        }
        outcome
    }

    fn send_frame(output: &SharedOutput, frame: &BridgeFrame) -> io::Result<()> {
        let mut encoded = Vec::with_capacity(frame.payload.len() + super::FRAME_HEADER_LENGTH);
        super::encode_frame(frame, &mut encoded);
        let mut output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        output.write_all(&encoded)?;
        output.flush()
    }

    fn accept_connections(
        listener: UnixListener,
        connections: &Connections,
        output: &SharedOutput,
    ) {
        let mut next_connection_id: u32 = 1;
        loop {
            let stream = match listener.accept() {
                Ok((stream, _address)) => stream,
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        || error.raw_os_error() == Some(libc::ECONNABORTED) =>
                {
                    continue;
                }
                Err(error) => {
                    log::debug!("[ide-bridge] stopped accepting: {error}");
                    return;
                }
            };
            let reader = match stream.try_clone() {
                Ok(reader) => reader,
                Err(error) => {
                    log::warn!("[ide-bridge] could not duplicate a connection: {error}");
                    continue;
                }
            };
            // A program that opens a connection and then never reads it would
            // otherwise leave this connection's writer blocked in `write_all`
            // for good, holding a thread and one of `MAX_IDE_CONNECTIONS` —
            // and, since Zed's Close is now ordered behind what is queued,
            // nothing would take it away either. Sixty seconds to write to a
            // socket on the same machine is a pathological state, not a slow
            // one; reaching it ends this connection alone
            // ([`forward_to_connection`]), and the bridge goes on serving the
            // others.
            if let Err(error) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
                log::warn!("[ide-bridge] could not bound a connection's writes: {error}");
                if let Err(error) = stream.shutdown(Shutdown::Both) {
                    log::debug!("[ide-bridge] ending an unbounded connection: {error}");
                }
                continue;
            }
            let stream = Arc::new(stream);
            // Unbounded as a channel, bounded in bytes by `forward_from_zed`.
            let (writer, payloads) = std::sync::mpsc::channel::<Vec<u8>>();
            let queued_bytes = Arc::new(AtomicUsize::new(0));
            let connection_id = {
                let mut table = lock_connections(connections);
                if table.len() >= MAX_IDE_CONNECTIONS {
                    // Over the cap: close it at once without announcing it.
                    log::debug!(
                        "[ide-bridge] refusing a connection: already serving {MAX_IDE_CONNECTIONS}"
                    );
                    drop(table);
                    if let Err(error) = stream.shutdown(Shutdown::Both) {
                        log::debug!("[ide-bridge] ending a refused connection: {error}");
                    }
                    continue;
                }
                while next_connection_id == 0 || table.contains_key(&next_connection_id) {
                    next_connection_id = next_connection_id.wrapping_add(1);
                }
                let connection_id = next_connection_id;
                next_connection_id = next_connection_id.wrapping_add(1);
                table.insert(
                    connection_id,
                    Connection {
                        stream: stream.clone(),
                        writer,
                        queued_bytes: queued_bytes.clone(),
                    },
                );
                connection_id
            };
            if let Err(error) = send_frame(output, &BridgeFrame::open(connection_id)) {
                // Zed is gone; reading its input notices that on its own.
                log::debug!("[ide-bridge] could not announce a connection: {error}");
                return;
            }
            // The writer thread drains Zed's Data frames onto the socket.
            let writer_thread = std::thread::Builder::new()
                .name(format!("zed-ide-bridge-{connection_id}-w"))
                .spawn({
                    let stream = stream.clone();
                    let connections = connections.clone();
                    let output = output.clone();
                    move || {
                        forward_to_connection(
                            connection_id,
                            &stream,
                            payloads,
                            &queued_bytes,
                            &connections,
                            &output,
                        )
                    }
                });
            if let Err(error) = writer_thread {
                log::warn!("[ide-bridge] could not serve connection {connection_id}: {error}");
                close_connection(connection_id, connections, output);
                continue;
            }
            // The reader thread carries the socket's bytes back to Zed.
            let reader_thread = std::thread::Builder::new()
                .name(format!("zed-ide-bridge-{connection_id}-r"))
                .spawn({
                    let connections = connections.clone();
                    let output = output.clone();
                    move || forward_to_zed(connection_id, reader, &connections, &output)
                });
            if let Err(error) = reader_thread {
                log::warn!("[ide-bridge] could not serve connection {connection_id}: {error}");
                close_connection(connection_id, connections, output);
            }
        }
    }

    /// Drain Zed's Data frames for one connection onto its socket. Ends when
    /// the queue's sender is dropped (the connection was closed), when a write
    /// fails, or when one takes longer than [`WRITE_TIMEOUT`] because the
    /// program stopped reading; in every case the connection is closed and the
    /// others go on.
    fn forward_to_connection(
        connection_id: u32,
        stream: &UnixStream,
        payloads: std::sync::mpsc::Receiver<Vec<u8>>,
        queued_bytes: &AtomicUsize,
        connections: &Connections,
        output: &SharedOutput,
    ) {
        for payload in payloads {
            let mut writer: &UnixStream = stream;
            let written = writer.write_all(&payload);
            queued_bytes.fetch_sub(payload.len(), Ordering::SeqCst);
            if let Err(error) = written {
                // `WouldBlock` here is the write timeout: the program has not
                // taken what Zed sent for a minute. Nothing can be resumed
                // from a half-written frame anyway, so the connection ends.
                log::debug!("[ide-bridge] connection {connection_id} stopped taking data: {error}");
                break;
            }
        }
        // Everything Zed sent for this connection is on the socket now, so the
        // half-close is what tells the other end there is no more — and it is
        // ordered after the bytes, which a `shutdown` from the frame reader
        // would not be (see the `Close` arm of `forward_from_zed`). A client
        // that reads until the end of its answer, as an HTTP one does, needs
        // exactly that.
        if let Err(error) = stream.shutdown(Shutdown::Write) {
            log::debug!("[ide-bridge] half-closing connection {connection_id}: {error}");
        }
        close_connection(connection_id, connections, output);
    }

    fn forward_to_zed(
        connection_id: u32,
        mut stream: UnixStream,
        connections: &Connections,
        output: &SharedOutput,
    ) {
        let mut buffer = vec![0; MAX_BRIDGE_FRAME_PAYLOAD];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let Some(bytes) = buffer.get(..read) else {
                        break;
                    };
                    if send_frame(output, &BridgeFrame::data(connection_id, bytes.to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    log::debug!("[ide-bridge] connection {connection_id} ended: {error}");
                    break;
                }
            }
        }
        close_connection(connection_id, connections, output);
    }

    /// End a connection and tell Zed — unless Zed closed it first, in which
    /// case it is no longer in `connections`. Dropping the removed
    /// [`Connection`] drops its writer queue's sender, so the writer thread
    /// ends too.
    fn close_connection(connection_id: u32, connections: &Connections, output: &SharedOutput) {
        let removed = lock_connections(connections).remove(&connection_id);
        if let Some(connection) = removed {
            if let Err(error) = connection.stream.shutdown(Shutdown::Both) {
                log::debug!("[ide-bridge] ending connection {connection_id}: {error}");
            }
            if let Err(error) = send_frame(output, &BridgeFrame::close(connection_id)) {
                log::debug!(
                    "[ide-bridge] could not report connection {connection_id} closed: {error}"
                );
            }
        }
    }

    fn forward_from_zed(
        mut input: impl Read,
        connections: &Connections,
        output: &SharedOutput,
    ) -> Result<()> {
        let mut decoder = BridgeFrameDecoder::new();
        let mut buffer = vec![0; MAX_BRIDGE_FRAME_PAYLOAD];
        loop {
            let read = match input.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("reading from Zed"),
            };
            if read == 0 {
                return decoder.finish();
            }
            let bytes = buffer.get(..read).context("bug: read past the buffer")?;
            for frame in decoder.push(bytes)? {
                match frame.kind {
                    BridgeFrameKind::Open => bail!(
                        "Zed opened connection {}, but only the bridge opens connections",
                        frame.connection_id
                    ),
                    BridgeFrameKind::Data => {
                        let queue = lock_connections(connections).get(&frame.connection_id).map(
                            |connection| {
                                (connection.writer.clone(), connection.queued_bytes.clone())
                            },
                        );
                        // Gone: it was closed while this frame was on its way.
                        let Some((writer, queued_bytes)) = queue else {
                            continue;
                        };
                        // Queued without blocking the shared pipe. One that
                        // would take the connection past its budget means the
                        // sandboxed reader stalled: that one connection is
                        // closed rather than the others held up. Only this
                        // loop adds, so the total cannot pass the budget
                        // between the check and the addition.
                        let length = frame.payload.len();
                        if queued_bytes.load(Ordering::SeqCst).saturating_add(length)
                            > WRITER_BYTES_PER_CONNECTION
                        {
                            log::debug!(
                                "[ide-bridge] connection {} is not reading; closing it",
                                frame.connection_id
                            );
                            close_connection(frame.connection_id, connections, output);
                            continue;
                        }
                        queued_bytes.fetch_add(length, Ordering::SeqCst);
                        // A failure means the writer thread is gone; it closes
                        // the connection itself.
                        if writer.send(frame.payload).is_err() {
                            log::debug!(
                                "[ide-bridge] connection {} has no writer any more",
                                frame.connection_id
                            );
                        }
                    }
                    BridgeFrameKind::Close => {
                        // Taking it out of the table drops its writer queue's
                        // sender, so the writer thread writes what is still
                        // queued and half-closes the socket only then (see
                        // [`forward_to_connection`]). Closing the socket here
                        // instead would throw those bytes away — and for a
                        // client that closes its connection after every answer
                        // (an MCP server over streamable HTTP, ADR-F49 決定 24)
                        // that is the very answer it is waiting for. Reading is
                        // shut down at once all the same, so a peer that never
                        // reads cannot keep the connection past Zed's Close.
                        let removed = lock_connections(connections).remove(&frame.connection_id);
                        if let Some(connection) = removed
                            && let Err(error) = connection.stream.shutdown(Shutdown::Read)
                        {
                            log::debug!(
                                "[ide-bridge] ending connection {}: {error}",
                                frame.connection_id
                            );
                        }
                    }
                }
            }
        }
    }

    /// The directory an entry lives in, and the entry's name, for a path that
    /// must name one plainly: absolute, no `..`, a short final component.
    fn split_entry_path(path: &Path) -> Result<(PathBuf, CString)> {
        ensure!(
            path.is_absolute(),
            "{} is not an absolute path",
            path.display()
        );
        ensure!(
            !path
                .components()
                .any(|component| component == Component::ParentDir),
            "{} contains `..`",
            path.display()
        );
        let Some(Component::Normal(name)) = path.components().next_back() else {
            bail!("{} does not name an entry of a directory", path.display());
        };
        ensure!(
            name.len() <= MAX_ENTRY_NAME_LENGTH,
            "the name of {} is longer than {MAX_ENTRY_NAME_LENGTH} bytes",
            path.display()
        );
        let directory = path
            .parent()
            .with_context(|| format!("{} has no directory", path.display()))?
            .to_path_buf();
        let name = CString::new(name.as_bytes())
            .with_context(|| format!("{} contains a NUL byte", path.display()))?;
        Ok((directory, name))
    }

    #[derive(Clone, Copy, PartialEq)]
    enum DirectoryAccess {
        /// Owner-only; tightened to that if it is ours but wider.
        Private,
        /// Nobody else may add or replace entries; tightened to owner-only if
        /// our group may.
        NotWritableByOthers,
    }

    /// A directory [`open_directory_tree`] reached.
    struct OpenedDirectory {
        directory: OwnedFd,
        /// When the walk created the directory itself: the directory holding
        /// it and its name there, to remove it again by.
        created: Option<(OwnedFd, CString)>,
    }

    /// Open the directory `target` after walking to it one component at a time
    /// from `base` — a trusted directory `target` must be under (the account's
    /// home in production) — never following a symlink, creating missing
    /// components owner-only, and checking each component is a real directory of
    /// this account's.
    ///
    /// Opening the whole path at once with `O_NOFOLLOW` only refuses a symlink
    /// at the *last* component; an intermediate component could be a symlink
    /// another process swapped in, which is how a lock or socket ends up
    /// redirected. Walking per component with `openat(O_NOFOLLOW | O_DIRECTORY)`
    /// / `mkdirat` and checking the owner of each closes that (ADR-F49 item 3):
    /// a swap anywhere on the way fails instead of redirecting.
    fn open_directory_tree(
        target: &Path,
        base: &Path,
        access: DirectoryAccess,
    ) -> Result<OpenedDirectory> {
        let canonical_base = std::fs::canonicalize(base)
            .with_context(|| format!("resolving the trusted base directory {}", base.display()))?;
        // Spelled through the base as given — a home reached through a
        // symlink, say — or through what it resolves to.
        let relative = target
            .strip_prefix(base)
            .or_else(|_| target.strip_prefix(&canonical_base))
            .map_err(|_| {
                anyhow::anyhow!(
                    "{} is not inside the trusted base directory {}",
                    target.display(),
                    base.display()
                )
            })?;
        // The base and everything above it are the account's own home and the
        // system directories over it: the trusted starting point, opened
        // following symlinks (it is canonical, so there are none to follow).
        let base_path = CString::new(canonical_base.as_os_str().as_bytes())
            .with_context(|| format!("{} contains a NUL byte", canonical_base.display()))?;
        // SAFETY: `base_path` is NUL-terminated; a non-creating open takes no
        // mode.
        let raw = unsafe {
            libc::open(
                base_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error()).with_context(|| {
                format!(
                    "opening the trusted base directory {}",
                    canonical_base.display()
                )
            });
        }
        // SAFETY: freshly opened by us and owned by nothing else.
        let mut directory = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let own_uid = unsafe { libc::geteuid() };
        let mut created = None;
        let mut walked = canonical_base.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                bail!("{} has an unexpected path component", target.display());
            };
            ensure!(
                name.len() <= MAX_ENTRY_NAME_LENGTH,
                "a component of {} is longer than {MAX_ENTRY_NAME_LENGTH} bytes",
                target.display()
            );
            let name = CString::new(name.as_bytes())
                .with_context(|| format!("{} contains a NUL byte", target.display()))?;
            walked.push(OsStr::from_bytes(name.as_bytes()));
            let (child, child_created) =
                open_or_create_subdirectory(&directory, &name, &walked, own_uid)?;
            let parent = std::mem::replace(&mut directory, child);
            created = child_created.then_some((parent, name));
        }
        apply_directory_access(&directory, &walked, access)?;
        Ok(OpenedDirectory { directory, created })
    }

    /// Open `name` in `parent` as a directory we own — refusing a symlink —
    /// creating it owner-only if absent. Returns the descriptor and whether it
    /// was created.
    ///
    /// A directory of ours others in our group may write to is fine on the way
    /// (Ubuntu's `umask 002` makes `~/.cache` like that): whatever such a
    /// writer puts in the next component's place is either a symlink or not
    /// ours, and the walk stops there. One any account may write to is not.
    fn open_or_create_subdirectory(
        parent: &OwnedFd,
        name: &CStr,
        display: &Path,
        own_uid: libc::uid_t,
    ) -> Result<(OwnedFd, bool)> {
        let mut created = false;
        let directory = loop {
            // SAFETY: `parent` is open and `name` is NUL-terminated; a
            // non-creating open takes no mode.
            let raw = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw >= 0 {
                // SAFETY: freshly opened by us and owned by nothing else.
                break unsafe { OwnedFd::from_raw_fd(raw) };
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound && !created {
                // SAFETY: `parent` is open and `name` is NUL-terminated.
                if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    let created_error = io::Error::last_os_error();
                    if created_error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(created_error)
                            .with_context(|| format!("creating {}", display.display()));
                    }
                }
                created = true;
                continue;
            }
            return Err(error).with_context(|| {
                format!(
                    "opening {} (it must be a real directory, not a symlink)",
                    display.display()
                )
            });
        };
        let status = fd_status(directory.as_fd())
            .with_context(|| format!("examining {}", display.display()))?;
        ensure!(
            status.is_directory(),
            "{} is not a directory",
            display.display()
        );
        ensure!(
            status.owner == own_uid,
            "{} belongs to uid {}, not to this account ({own_uid})",
            display.display(),
            status.owner
        );
        ensure!(
            status.mode & 0o002 == 0,
            "{} is writable by every account",
            display.display()
        );
        Ok((directory, created))
    }

    /// Hold the final directory to the requested access by tightening it to
    /// owner-only: a `Private` one when anyone else may use it at all, a
    /// `NotWritableByOthers` one when our group may write to it (Ubuntu's
    /// `umask 002`). The walk has already refused one that is not ours or that
    /// every account may write to.
    fn apply_directory_access(
        directory: &OwnedFd,
        display: &Path,
        access: DirectoryAccess,
    ) -> Result<()> {
        let status = fd_status(directory.as_fd())
            .with_context(|| format!("examining {}", display.display()))?;
        let too_open = match access {
            DirectoryAccess::Private => status.mode & 0o077 != 0,
            DirectoryAccess::NotWritableByOthers => status.mode & 0o022 != 0,
        };
        // SAFETY: `fchmod` on the descriptor the walk checked, so it is that
        // same directory whatever its path now leads to.
        if too_open && unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("making {} owner-only", display.display()));
        }
        Ok(())
    }

    struct EntryStatus {
        mode: libc::mode_t,
        owner: libc::uid_t,
        identity: (u64, u64),
    }

    impl EntryStatus {
        fn from_stat(stat: &libc::stat) -> Self {
            Self {
                mode: stat.st_mode,
                owner: stat.st_uid,
                identity: (stat.st_dev, stat.st_ino),
            }
        }

        fn is_directory(&self) -> bool {
            self.mode & libc::S_IFMT == libc::S_IFDIR
        }

        fn is_socket(&self) -> bool {
            self.mode & libc::S_IFMT == libc::S_IFSOCK
        }
    }

    fn fd_status(fd: BorrowedFd<'_>) -> io::Result<EntryStatus> {
        // SAFETY: an all-zero `stat` is a valid value for `fstat` to fill.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open and `stat` is a valid out-parameter.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(EntryStatus::from_stat(&stat))
    }

    /// What `name` in `directory` is, without following a symlink; `None`
    /// when there is nothing.
    fn entry_status(directory: &OwnedFd, name: &CStr) -> io::Result<Option<EntryStatus>> {
        // SAFETY: an all-zero `stat` is a valid value for `fstatat` to fill.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `directory` is open, `name` is NUL-terminated, and `stat` is
        // a valid out-parameter.
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(EntryStatus::from_stat(&stat)))
    }

    fn unlink_entry(directory: &OwnedFd, name: &CStr) -> io::Result<()> {
        // SAFETY: `directory` is open and `name` is NUL-terminated.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Remove `name` from `directory` only while it is still the entry with
    /// `identity`: whatever took its place is not ours to delete.
    fn remove_if_unchanged(directory: &OwnedFd, name: &CStr, identity: (u64, u64)) {
        match entry_status(directory, name) {
            Ok(Some(status)) if status.identity == identity => {
                if let Err(error) = unlink_entry(directory, name) {
                    log::warn!("[ide-bridge] could not remove {name:?}: {error}");
                }
            }
            Ok(_) => {}
            Err(error) => log::warn!("[ide-bridge] could not examine {name:?}: {error}"),
        }
    }

    /// The rendezvous socket, in a private directory of this account's.
    struct Rendezvous {
        directory: OwnedFd,
        /// The directory holding `directory` and its name there, when the
        /// bridge created `directory` and so removes it again.
        created_directory: Option<(OwnedFd, CString)>,
        socket_name: CString,
        socket_identity: (u64, u64),
    }

    impl Rendezvous {
        fn create(path: &Path, base: &Path) -> Result<(Self, UnixListener)> {
            let (directory_path, socket_name) = split_entry_path(path)?;
            let OpenedDirectory {
                directory,
                created: created_directory,
            } = open_directory_tree(&directory_path, base, DirectoryAccess::Private)?;
            // SAFETY: `geteuid` takes no arguments and cannot fail.
            let own_uid = unsafe { libc::geteuid() };
            match entry_status(&directory, &socket_name)
                .with_context(|| format!("examining {}", path.display()))?
            {
                None => {}
                // Left behind by a bridge that did not get to clean up.
                Some(status) if status.is_socket() && status.owner == own_uid => {
                    unlink_entry(&directory, &socket_name)
                        .with_context(|| format!("removing the stale {}", path.display()))?;
                }
                Some(_) => bail!(
                    "{} exists and is not a socket of this account; refusing to replace it",
                    path.display()
                ),
            }
            // Bound through the directory descriptor, so it lands in the
            // directory that was just checked whatever happens to its path.
            let bind_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
                .join(OsStr::from_bytes(socket_name.as_bytes()));
            let listener = UnixListener::bind(&bind_path)
                .with_context(|| format!("listening on {}", path.display()))?;
            let status = entry_status(&directory, &socket_name)
                .with_context(|| format!("examining {}", path.display()))?
                .with_context(|| format!("{} vanished right after it was bound", path.display()))?;
            ensure!(
                status.is_socket(),
                "{} is not the socket just bound",
                path.display()
            );
            Ok((
                Self {
                    directory,
                    created_directory,
                    socket_name,
                    socket_identity: status.identity,
                },
                listener,
            ))
        }

        fn remove(&self) {
            remove_if_unchanged(&self.directory, &self.socket_name, self.socket_identity);
            let Some((parent, name)) = &self.created_directory else {
                return;
            };
            // Only while the name still leads to the directory created here,
            // and only once it is empty (`rmdir` refuses anything else).
            let still_ours = match (
                fd_status(self.directory.as_fd()),
                entry_status(parent, name),
            ) {
                (Ok(ours), Ok(Some(current))) => ours.identity == current.identity,
                _ => false,
            };
            // SAFETY: `parent` is open and `name` is NUL-terminated.
            if still_ours
                && unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                    != 0
            {
                log::debug!(
                    "[ide-bridge] leaving the rendezvous directory {name:?}: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }

    /// The IDE lock file, created exclusively and never through a symlink.
    struct LockFile {
        directory: OwnedFd,
        name: CString,
        /// Kept open while the bridge runs: a deleted file's inode number is
        /// free for the next file to take, and then the identity check in
        /// [`LockFile::remove`] would take someone else's file for ours.
        file: std::fs::File,
    }

    impl LockFile {
        fn create(path: &Path, content: &[u8], base: &Path) -> Result<Self> {
            let (directory_path, name) = split_entry_path(path)?;
            let directory =
                open_directory_tree(&directory_path, base, DirectoryAccess::NotWritableByOthers)?
                    .directory;
            // SAFETY: `directory` is open, `name` is NUL-terminated, and the
            // mode is passed because the open may create the file.
            let raw = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if raw < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    bail!(
                        "{} already exists (a symlink there is refused as well)",
                        path.display()
                    );
                }
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
            // SAFETY: freshly opened by us and owned by nothing else.
            let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
            let lock_file = Self {
                directory,
                name,
                file,
            };
            let mut writer = &lock_file.file;
            if let Err(error) = writer.write_all(content) {
                lock_file.remove();
                return Err(error).with_context(|| format!("writing {}", path.display()));
            }
            Ok(lock_file)
        }

        fn remove(&self) {
            match fd_status(self.file.as_fd()) {
                Ok(status) => remove_if_unchanged(&self.directory, &self.name, status.identity),
                Err(error) => log::warn!("[ide-bridge] could not examine the lock file: {error}"),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        use std::time::Duration;

        /// Reads frames off a blocking stream, as Zed would.
        struct FrameReader {
            stream: UnixStream,
            decoder: BridgeFrameDecoder,
            frames: std::collections::VecDeque<BridgeFrame>,
        }

        impl FrameReader {
            fn new(stream: UnixStream) -> Self {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("set a read timeout");
                Self {
                    stream,
                    decoder: BridgeFrameDecoder::new(),
                    frames: Default::default(),
                }
            }

            fn next(&mut self) -> BridgeFrame {
                let mut buffer = [0u8; 4096];
                while self.frames.is_empty() {
                    let read = self.stream.read(&mut buffer).expect("read a frame");
                    assert_ne!(read, 0, "the bridge output ended");
                    self.frames
                        .extend(self.decoder.push(&buffer[..read]).expect("valid frames"));
                }
                self.frames.pop_front().expect("a frame")
            }
        }

        fn send(stream: &mut UnixStream, frame: &BridgeFrame) {
            let mut encoded = Vec::new();
            super::super::encode_frame(frame, &mut encoded);
            stream.write_all(&encoded).expect("write a frame");
        }

        fn read_exactly(stream: &mut UnixStream, length: usize) -> Vec<u8> {
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("set a read timeout");
            let mut bytes = vec![0; length];
            stream
                .read_exact(&mut bytes)
                .expect("read from the connection");
            bytes
        }

        /// A listener, and the two ends of the bridge's stdio as Zed holds
        /// them, with `serve_bridge` running on the other ends.
        fn start_bridge(
            socket: &Path,
        ) -> (UnixStream, FrameReader, std::thread::JoinHandle<Result<()>>) {
            let listener = UnixListener::bind(socket).expect("bind the rendezvous");
            let (zed_input, bridge_input) = UnixStream::pair().expect("input pair");
            let (bridge_output, zed_output) = UnixStream::pair().expect("output pair");
            let bridge =
                std::thread::spawn(move || serve_bridge(listener, bridge_input, bridge_output));
            (zed_input, FrameReader::new(zed_output), bridge)
        }

        #[test]
        fn connections_are_relayed_both_ways_and_closed_from_either_side() {
            let root = tempfile::tempdir().expect("tempdir");
            let socket = root.path().join("bridge.sock");
            let (mut zed_input, mut zed_output, bridge) = start_bridge(&socket);

            let mut first = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(1));
            first.write_all(b"ping").expect("write");
            assert_eq!(zed_output.next(), BridgeFrame::data(1, b"ping".to_vec()));
            send(&mut zed_input, &BridgeFrame::data(1, b"pong".to_vec()));
            assert_eq!(read_exactly(&mut first, 4), b"pong");

            let mut second = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(2));
            send(&mut zed_input, &BridgeFrame::data(2, b"two".to_vec()));
            assert_eq!(read_exactly(&mut second, 3), b"two");

            // Closed by the program: Zed hears of it.
            first.shutdown(Shutdown::Write).expect("half-close");
            assert_eq!(zed_output.next(), BridgeFrame::close(1));
            // Closed by Zed: the program sees the end, and no Close comes back.
            send(&mut zed_input, &BridgeFrame::close(2));
            let mut rest = Vec::new();
            second.read_to_end(&mut rest).expect("read to the end");
            assert!(rest.is_empty());
            // Data for a connection that is gone is dropped, not an error.
            send(&mut zed_input, &BridgeFrame::data(2, b"late".to_vec()));

            let mut third = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(3));
            third.write_all(b"3").expect("write");
            assert_eq!(zed_output.next(), BridgeFrame::data(3, b"3".to_vec()));

            drop(zed_input);
            bridge
                .join()
                .expect("the bridge thread")
                .expect("the end of Zed's input is a clean stop");
            let mut rest = Vec::new();
            third
                .read_to_end(&mut rest)
                .expect("the bridge ends open connections");
        }

        #[test]
        fn a_burst_of_small_frames_behind_a_full_socket_is_all_delivered() {
            let root = tempfile::tempdir().expect("tempdir");
            let socket = root.path().join("bridge.sock");
            let (mut zed_input, mut zed_output, bridge) = start_bridge(&socket);
            let mut client = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(1));

            // Enough full frames to fill the socket and leave the writer
            // blocked on it, still well within the budget; then far more small
            // frames than any count-based limit would take.
            let mut expected = Vec::new();
            for _ in 0..8 {
                let payload = vec![b'a'; MAX_BRIDGE_FRAME_PAYLOAD];
                expected.extend_from_slice(&payload);
                send(&mut zed_input, &BridgeFrame::data(1, payload));
            }
            for index in 0..500u32 {
                let payload = format!("{index:04}").into_bytes();
                expected.extend_from_slice(&payload);
                send(&mut zed_input, &BridgeFrame::data(1, payload));
            }
            assert!(
                read_exactly(&mut client, expected.len()) == expected,
                "the bytes arrive complete and in order"
            );
            client.write_all(b"done").expect("write");
            assert_eq!(
                zed_output.next(),
                BridgeFrame::data(1, b"done".to_vec()),
                "still open: no Close came first"
            );
            drop(client);
            drop(zed_input);
            bridge
                .join()
                .expect("the bridge thread")
                .expect("clean stop");
        }

        /// ADR-F49 決定 24: an answer Zed sends and then closes — which is
        /// every answer of an MCP server over streamable HTTP, since it closes
        /// the connection with it — reaches the program whole, and its end
        /// arrives as a half-close instead of as bytes thrown away. The Close
        /// must not overtake what the connection's writer still has queued.
        #[test]
        fn the_answer_before_a_close_still_reaches_the_program() {
            let root = tempfile::tempdir().expect("tempdir");
            let socket = root.path().join("bridge.sock");
            let (mut zed_input, mut zed_output, bridge) = start_bridge(&socket);
            let mut client = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(1));

            // More than the socket takes at once, so the writer is still
            // draining when the Close arrives, and well inside its budget.
            let mut answer = Vec::new();
            for _ in 0..8 {
                let payload = vec![b'z'; MAX_BRIDGE_FRAME_PAYLOAD];
                answer.extend_from_slice(&payload);
                send(&mut zed_input, &BridgeFrame::data(1, payload));
            }
            send(&mut zed_input, &BridgeFrame::close(1));

            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("set a read timeout");
            let mut received = Vec::new();
            client
                .read_to_end(&mut received)
                .expect("read the whole answer");
            assert!(
                received == answer,
                "all {} bytes arrive before the end of the connection, not {}",
                answer.len(),
                received.len()
            );

            drop(client);
            drop(zed_input);
            bridge
                .join()
                .expect("the bridge thread")
                .expect("clean stop");
        }

        /// ADR-F49: a program that opens a connection and then never reads it
        /// must not keep its writer thread — and one of the connection slots —
        /// for good. [`WRITE_TIMEOUT`] on the socket is the way out; only that
        /// connection ends, and Zed is told it did.
        ///
        /// Driven straight at [`forward_to_connection`] with a short timeout,
        /// so the test does not wait the real minute out.
        #[test]
        fn a_connection_that_is_never_read_lets_its_writer_go() {
            let (_peer, bridge_end) = UnixStream::pair().expect("a pair");
            bridge_end
                .set_write_timeout(Some(Duration::from_millis(200)))
                .expect("set a write timeout");
            let (zed_output, frames_to_zed) = UnixStream::pair().expect("an output pair");
            let output: SharedOutput = Arc::new(Mutex::new(Box::new(frames_to_zed)));
            let connections: Connections = Default::default();
            let (writer, payloads) = std::sync::mpsc::channel::<Vec<u8>>();
            let stream = Arc::new(bridge_end);
            let queued_bytes = Arc::new(AtomicUsize::new(0));
            lock_connections(&connections).insert(
                7,
                Connection {
                    stream: stream.clone(),
                    writer,
                    queued_bytes: queued_bytes.clone(),
                },
            );
            // `_peer` is never read, and this is far more than the socket
            // takes, so the writer is still blocked when the timeout fires.
            for _ in 0..8 {
                let payload = vec![b'z'; MAX_BRIDGE_FRAME_PAYLOAD];
                queued_bytes.fetch_add(payload.len(), Ordering::SeqCst);
                lock_connections(&connections)
                    .get(&7)
                    .expect("the connection")
                    .writer
                    .send(payload)
                    .expect("hand it to the writer");
            }

            let (done, finished) = std::sync::mpsc::channel::<()>();
            std::thread::spawn({
                let connections = connections.clone();
                let output = output.clone();
                move || {
                    forward_to_connection(
                        7,
                        &stream,
                        payloads,
                        &queued_bytes,
                        &connections,
                        &output,
                    );
                    done.send(()).ok();
                }
            });
            finished
                .recv_timeout(Duration::from_secs(10))
                .expect("the writer gives up instead of blocking for good");

            let mut zed = FrameReader::new(zed_output);
            assert_eq!(zed.next(), BridgeFrame::close(7));
            assert!(
                lock_connections(&connections).is_empty(),
                "and the connection's slot is free again"
            );
        }

        #[test]
        fn a_stalled_connection_is_closed_without_blocking_the_others() {
            let root = tempfile::tempdir().expect("tempdir");
            let socket = root.path().join("bridge.sock");
            let (mut zed_input, mut zed_output, bridge) = start_bridge(&socket);

            // `stalled` is never read; `working` is.
            let stalled = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(1));
            let mut working = UnixStream::connect(&socket).expect("connect");
            assert_eq!(zed_output.next(), BridgeFrame::open(2));

            // Push well past the socket buffers plus the ~1 MiB writer queue of
            // the stalled connection.
            for _ in 0..64 {
                send(
                    &mut zed_input,
                    &BridgeFrame::data(1, vec![0u8; MAX_BRIDGE_FRAME_PAYLOAD]),
                );
            }
            // The working connection still round-trips both ways.
            send(&mut zed_input, &BridgeFrame::data(2, b"ping".to_vec()));
            assert_eq!(read_exactly(&mut working, 4), b"ping");
            working.write_all(b"pong").expect("write");

            let mut saw_close = false;
            let mut saw_pong = false;
            while !(saw_close && saw_pong) {
                match zed_output.next() {
                    frame if frame == BridgeFrame::close(1) => saw_close = true,
                    frame if frame == BridgeFrame::data(2, b"pong".to_vec()) => saw_pong = true,
                    _ => {}
                }
            }
            drop(stalled);
            drop(zed_input);
            bridge
                .join()
                .expect("the bridge thread")
                .expect("clean stop");
        }

        #[test]
        fn no_more_than_the_cap_are_served_at_once() {
            use super::MAX_IDE_CONNECTIONS;
            use std::time::Duration;
            let root = tempfile::tempdir().expect("tempdir");
            let socket = root.path().join("bridge.sock");
            let (zed_input, mut zed_output, bridge) = start_bridge(&socket);

            // Hold open the cap's worth; each gets an Open.
            let mut held = Vec::new();
            for expected in 1..=MAX_IDE_CONNECTIONS as u32 {
                held.push(UnixStream::connect(&socket).expect("connect"));
                assert_eq!(zed_output.next(), BridgeFrame::open(expected));
            }
            // One more is accepted and closed at once, with no Open frame.
            let mut extra = UnixStream::connect(&socket).expect("connect");
            extra
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("set a read timeout");
            let mut byte = [0u8; 1];
            assert_eq!(
                extra.read(&mut byte).expect("read the refused connection"),
                0,
                "the connection beyond the cap is closed at once"
            );
            drop(held);
            drop(zed_input);
            bridge
                .join()
                .expect("the bridge thread")
                .expect("clean stop");
        }

        #[test]
        fn zed_may_not_open_connections() {
            let root = tempfile::tempdir().expect("tempdir");
            let (mut zed_input, _zed_output, bridge) = start_bridge(&root.path().join("s.sock"));
            send(&mut zed_input, &BridgeFrame::open(7));
            assert!(bridge.join().expect("the bridge thread").is_err());
        }

        #[test]
        fn input_ending_inside_a_frame_is_an_error() {
            let root = tempfile::tempdir().expect("tempdir");
            let (mut zed_input, _zed_output, bridge) = start_bridge(&root.path().join("s.sock"));
            zed_input
                .write_all(&[2, 1, 0])
                .expect("write half a header");
            drop(zed_input);
            assert!(bridge.join().expect("the bridge thread").is_err());
        }

        #[test]
        fn the_rendezvous_is_private_and_replaces_only_its_own_stale_socket() {
            let root = tempfile::tempdir().expect("tempdir");
            let base = root.path();
            let socket = base.join("ide/session/bridge.sock");

            let (rendezvous, listener) = Rendezvous::create(&socket, base).expect("create");
            let directory = std::fs::metadata(socket.parent().expect("parent")).expect("stat");
            assert_eq!(directory.permissions().mode() & 0o777, 0o700);
            assert!(UnixStream::connect(&socket).is_ok());
            drop(listener);
            rendezvous.remove();
            assert!(!socket.exists(), "the socket is removed");
            assert!(
                !socket.parent().expect("parent").exists(),
                "the directory the bridge created is removed"
            );

            // A socket some earlier bridge left behind is replaced.
            std::fs::create_dir_all(socket.parent().expect("parent")).expect("create");
            drop(UnixListener::bind(&socket).expect("a stale socket"));
            let (rendezvous, _listener) = Rendezvous::create(&socket, base).expect("replace it");
            assert!(UnixStream::connect(&socket).is_ok());
            rendezvous.remove();

            // Anything else is left alone.
            std::fs::write(&socket, "not a socket").expect("write");
            assert!(Rendezvous::create(&socket, base).is_err());
            assert_eq!(
                std::fs::read_to_string(&socket).expect("read"),
                "not a socket"
            );
        }

        #[test]
        fn the_rendezvous_directory_may_not_be_a_symlink() {
            let root = tempfile::tempdir().expect("tempdir");
            let real = root.path().join("real");
            std::fs::create_dir(&real).expect("create");
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            let link = root.path().join("link");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            // A symlink anywhere on the walk from the base is refused.
            assert!(Rendezvous::create(&link.join("bridge.sock"), root.path()).is_err());
            assert!(!real.join("bridge.sock").exists());
        }

        #[test]
        fn a_rendezvous_directory_of_ours_is_made_owner_only() {
            let root = tempfile::tempdir().expect("tempdir");
            let directory = root.path().join("open");
            std::fs::create_dir(&directory).expect("create");
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            let (rendezvous, _listener) =
                Rendezvous::create(&directory.join("bridge.sock"), root.path()).expect("create");
            assert_eq!(
                std::fs::metadata(&directory).expect("stat").mode() & 0o777,
                0o700
            );
            rendezvous.remove();
            assert!(directory.exists(), "a directory it did not create stays");
        }

        #[test]
        fn the_lock_file_is_created_exclusively_and_never_through_a_symlink() {
            let root = tempfile::tempdir().expect("tempdir");
            let base = root.path();
            let directory = base.join("ide");
            let lock_path = directory.join("12345.lock");

            let lock_file = LockFile::create(&lock_path, b"{\"pid\":1}", base).expect("create");
            assert_eq!(std::fs::read(&lock_path).expect("read"), b"{\"pid\":1}");
            assert_eq!(
                std::fs::metadata(&lock_path).expect("stat").mode() & 0o777,
                0o600
            );
            assert!(
                LockFile::create(&lock_path, b"again", base).is_err(),
                "an existing lock file is not overwritten"
            );
            lock_file.remove();
            assert!(!lock_path.exists());

            let target = base.join("target");
            let planted = directory.join("23456.lock");
            std::os::unix::fs::symlink(&target, &planted).expect("symlink");
            assert!(LockFile::create(&planted, b"x", base).is_err());
            assert!(!target.exists(), "nothing is written through the symlink");

            let linked_directory = base.join("linked");
            std::os::unix::fs::symlink(&directory, &linked_directory).expect("symlink");
            assert!(LockFile::create(&linked_directory.join("34567.lock"), b"x", base).is_err());

            let open_directory = base.join("open");
            std::fs::create_dir(&open_directory).expect("create");
            std::fs::set_permissions(&open_directory, std::fs::Permissions::from_mode(0o777))
                .expect("chmod");
            assert!(
                LockFile::create(&open_directory.join("45678.lock"), b"x", base).is_err(),
                "a directory others can write to is refused"
            );
        }

        #[test]
        fn a_replaced_lock_file_is_not_removed() {
            let root = tempfile::tempdir().expect("tempdir");
            let lock_path = root.path().join("12345.lock");
            let lock_file = LockFile::create(&lock_path, b"ours", root.path()).expect("create");
            std::fs::remove_file(&lock_path).expect("remove");
            std::fs::write(&lock_path, "someone else's").expect("write");
            lock_file.remove();
            assert_eq!(
                std::fs::read_to_string(&lock_path).expect("read"),
                "someone else's"
            );
        }

        #[test]
        fn a_group_writable_directory_of_ours_on_the_way_is_accepted() {
            let root = tempfile::tempdir().expect("tempdir");
            let cache = root.path().join("cache");
            std::fs::create_dir(&cache).expect("create");
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o775))
                .expect("chmod");
            let socket = cache.join("ide/session/bridge.sock");
            let (rendezvous, _listener) = Rendezvous::create(&socket, root.path())
                .expect("umask 002 leaves ~/.cache like this");
            assert_eq!(
                std::fs::metadata(socket.parent().expect("parent"))
                    .expect("stat")
                    .mode()
                    & 0o777,
                0o700,
                "what the walk creates stays owner-only"
            );
            rendezvous.remove();
            assert!(!socket.parent().expect("parent").exists());

            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o777))
                .expect("chmod");
            assert!(
                Rendezvous::create(&socket, root.path()).is_err(),
                "a directory every account may write to is not"
            );
        }

        #[test]
        fn a_base_reached_through_a_symlink_is_walked_from_what_it_resolves_to() {
            let root = tempfile::tempdir().expect("tempdir");
            let real = root.path().join("real-home");
            std::fs::create_dir(&real).expect("create");
            let link = root.path().join("home");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            let lock_file = LockFile::create(&link.join("config/ide/1.lock"), b"x", &link)
                .expect("spelled through the link");
            assert_eq!(
                std::fs::read(real.join("config/ide/1.lock")).expect("read"),
                b"x"
            );
            lock_file.remove();
            let lock_file = LockFile::create(&real.join("config/ide/2.lock"), b"y", &link)
                .expect("spelled through what it resolves to");
            lock_file.remove();
        }

        #[test]
        fn a_group_writable_lock_directory_of_ours_is_tightened_not_refused() {
            let root = tempfile::tempdir().expect("tempdir");
            let directory = root.path().join("config/ide");
            std::fs::create_dir_all(&directory).expect("create");
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775))
                .expect("chmod");
            let lock_file = LockFile::create(&directory.join("1.lock"), b"x", root.path())
                .expect("what umask 002 leaves behind is usable");
            assert_eq!(
                std::fs::metadata(&directory).expect("stat").mode() & 0o777,
                0o700
            );
            lock_file.remove();
        }

        #[test]
        fn a_file_reached_only_through_a_symlinked_component_is_refused() {
            // An intermediate component being a symlink — not just the leaf —
            // is what the per-component walk closes (ADR-F49 item 3).
            let root = tempfile::tempdir().expect("tempdir");
            let real = root.path().join("real");
            std::fs::create_dir(&real).expect("create");
            let link = root.path().join("link");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            assert!(LockFile::create(&link.join("sub/1.lock"), b"x", root.path()).is_err());
            assert!(
                !real.join("sub").exists(),
                "nothing created past the symlink"
            );
        }

        #[test]
        fn one_bridge_at_a_time_holds_the_exclusive_directory() {
            let root = tempfile::tempdir().expect("tempdir");
            let directory = root.path().join("projects/0123456789abcdef");
            let claim = || claim_exclusive_directory(&directory, root.path()).expect("claim");
            let Claim::Held(first) = claim() else {
                panic!("nothing holds the directory yet");
            };
            assert_eq!(
                std::fs::metadata(&directory).expect("stat").mode() & 0o777,
                0o700,
                "the claim creates it owner-only"
            );
            assert!(
                matches!(claim(), Claim::Taken),
                "a second bridge is turned away"
            );
            drop(first);
            assert!(
                matches!(claim(), Claim::Held(_)),
                "the first one's end frees it"
            );

            let linked = root.path().join("linked");
            std::os::unix::fs::symlink(root.path().join("projects"), &linked).expect("symlink");
            assert!(
                claim_exclusive_directory(&linked.join("0123456789abcdef"), root.path()).is_err(),
                "never through a symlink"
            );
        }

        #[test]
        fn bridge_arguments_are_parsed_and_checked() {
            let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
            let options = parse_bridge_args(&args(&[
                "--rendezvous",
                "/r/bridge.sock",
                "--lock-file",
                "/c/ide/1.lock",
                "--exclusive-directory",
                "/c",
            ]))
            .expect("valid arguments");
            assert_eq!(options.rendezvous, PathBuf::from("/r/bridge.sock"));
            // The content no longer rides on the command line; only the path.
            assert_eq!(options.lock_file, Some(PathBuf::from("/c/ide/1.lock")));
            assert_eq!(options.exclusive_directory, Some(PathBuf::from("/c")));
            let bare = parse_bridge_args(&args(&["--rendezvous", "/r/s"])).expect("only a socket");
            assert!(bare.lock_file.is_none() && bare.exclusive_directory.is_none());
            for invalid in [
                &["--lock-file", "/c/1.lock"][..],
                &["--unknown"],
                &["--rendezvous", "/r/s", "--unknown"],
                &["--rendezvous"],
                &["--rendezvous", "/r/s", "--exclusive-directory"],
            ] {
                assert!(parse_bridge_args(&args(invalid)).is_err(), "{invalid:?}");
            }
        }

        #[test]
        fn entry_paths_must_be_plain() {
            assert!(split_entry_path(Path::new("relative/bridge.sock")).is_err());
            assert!(split_entry_path(Path::new("/a/../bridge.sock")).is_err());
            assert!(split_entry_path(Path::new("/")).is_err());
            assert!(split_entry_path(&Path::new("/a").join("x".repeat(65))).is_err());
            let (directory, name) = split_entry_path(Path::new("/a/b/bridge.sock")).expect("valid");
            assert_eq!(directory, PathBuf::from("/a/b"));
            assert_eq!(name.as_bytes(), b"bridge.sock");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::LocalPool;
    use futures::task::SpawnExt as _;

    /// ADR-F49 (new decision): the bridges a Zed runs at once are counted, one
    /// beyond the limit is turned away rather than trading an older one for
    /// it, and a bridge that ends gives its slot back.
    #[test]
    fn the_running_bridges_are_counted_and_capped() {
        let before = IdeBridgeSlot::running();
        let slots = (0..MAX_IDE_BRIDGES - before)
            .map(|_| IdeBridgeSlot::take().expect("a slot below the limit is given out"))
            .collect::<Vec<_>>();
        assert_eq!(IdeBridgeSlot::running(), MAX_IDE_BRIDGES);
        assert!(
            IdeBridgeSlot::take().is_none(),
            "the bridge past the limit is refused, and the ones already running stay"
        );
        assert_eq!(
            IdeBridgeSlot::running(),
            MAX_IDE_BRIDGES,
            "a refusal takes no slot of its own"
        );

        drop(slots);
        assert_eq!(IdeBridgeSlot::running(), before);
        let slot = IdeBridgeSlot::take();
        assert!(slot.is_some(), "a bridge that ended freed its slot");
    }

    /// The setup lines the bridge prints (in the distro) are read back by
    /// whoever started it (on Windows), and every failure code has a reason of
    /// its own.
    #[test]
    fn the_bridge_reports_readiness_and_setup_failures() {
        assert_eq!(
            parse_bridge_status("zed-ide-bridge: ready\n"),
            Some(BridgeStatus::Ready)
        );
        assert_eq!(
            parse_bridge_status(
                "zed-ide-bridge: lock-file-failed: creating 45123.lock: File exists"
            ),
            Some(BridgeStatus::Failed {
                code: "lock-file-failed"
            })
        );
        assert_eq!(
            parse_bridge_status("[ide-bridge] refusing a connection"),
            None
        );
        assert_eq!(parse_bridge_status("zed-ide-bridge: ready soon"), None);
        for code in [
            "bad-args",
            "signals-failed",
            "home-unknown",
            "rendezvous-failed",
            "lock-preamble-failed",
            "lock-file-failed",
        ] {
            let message = bridge_failure_message(code);
            assert!(
                !message.contains("不明な理由") && !message.contains(code),
                "{code}: {message}"
            );
        }
        assert!(bridge_failure_message("new-code").contains("new-code"));
        // The one the Windows side turns into "the session already runs".
        assert_eq!(
            parse_bridge_status("zed-ide-bridge: session-running: another IDE bridge serves /x"),
            Some(BridgeStatus::Failed {
                code: SESSION_RUNNING_CODE
            })
        );
    }

    #[test]
    fn frames_round_trip() {
        let frames = [
            BridgeFrame::open(1),
            BridgeFrame::data(1, b"hello".to_vec()),
            BridgeFrame::data(u32::MAX, Vec::new()),
            BridgeFrame::data(7, vec![0xab; MAX_BRIDGE_FRAME_PAYLOAD]),
            BridgeFrame::close(1),
        ];
        let mut encoded = Vec::new();
        for frame in &frames {
            encode_frame(frame, &mut encoded);
        }
        let mut decoder = BridgeFrameDecoder::new();
        assert_eq!(decoder.push(&encoded).expect("valid frames"), frames);
        decoder.finish().expect("nothing left over");
    }

    #[test]
    fn fragmented_input_decodes_the_same() {
        let frames = [
            BridgeFrame::open(3),
            BridgeFrame::data(3, (0..=255).collect()),
            BridgeFrame::close(3),
        ];
        let mut encoded = Vec::new();
        for frame in &frames {
            encode_frame(frame, &mut encoded);
        }
        let mut decoder = BridgeFrameDecoder::new();
        let mut decoded = Vec::new();
        for byte in &encoded {
            decoded.extend(
                decoder
                    .push(std::slice::from_ref(byte))
                    .expect("valid frames"),
            );
        }
        assert_eq!(decoded, frames);
        decoder.finish().expect("nothing left over");

        let mut decoder = BridgeFrameDecoder::new();
        let (first, second) = encoded.split_at(FRAME_HEADER_LENGTH + 4);
        assert_eq!(decoder.push(first).expect("valid"), [BridgeFrame::open(3)]);
        assert!(decoder.finish().is_err(), "the stream may not end here");
        assert_eq!(decoder.push(second).expect("valid").len(), 2);
    }

    #[test]
    fn an_oversized_frame_is_refused_before_its_payload_arrives() {
        let mut header = vec![2];
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&(MAX_BRIDGE_FRAME_PAYLOAD as u32 + 1).to_le_bytes());
        assert!(BridgeFrameDecoder::new().push(&header).is_err());
    }

    #[test]
    fn malformed_frames_are_protocol_errors() {
        let header = |kind: u8, length: u32| {
            let mut bytes = vec![kind];
            bytes.extend_from_slice(&5u32.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend(std::iter::repeat_n(0, length as usize));
            bytes
        };
        for bytes in [header(0, 0), header(4, 0), header(1, 1), header(3, 2)] {
            assert!(
                BridgeFrameDecoder::new().push(&bytes).is_err(),
                "{bytes:?} must be refused"
            );
        }
    }

    #[test]
    fn a_long_payload_is_split_into_frames() {
        let payload: Vec<u8> = (0..150_000).map(|index| (index % 251) as u8).collect();
        let mut encoded = Vec::new();
        encode_frame(&BridgeFrame::data(9, payload.clone()), &mut encoded);
        let frames = BridgeFrameDecoder::new()
            .push(&encoded)
            .expect("valid frames");
        assert_eq!(frames.len(), 3);
        assert!(
            frames
                .iter()
                .all(|frame| frame.kind == BridgeFrameKind::Data
                    && frame.connection_id == 9
                    && frame.payload.len() <= MAX_BRIDGE_FRAME_PAYLOAD)
        );
        let joined: Vec<u8> = frames.into_iter().flat_map(|frame| frame.payload).collect();
        assert_eq!(joined, payload);
    }

    /// One direction of an in-memory byte pipe.
    struct PipeWriter(mpsc::UnboundedSender<Vec<u8>>);

    struct PipeReader {
        receiver: mpsc::UnboundedReceiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    fn pipe() -> (PipeWriter, PipeReader) {
        let (sender, receiver) = mpsc::unbounded();
        (
            PipeWriter(sender),
            PipeReader {
                receiver,
                pending: Vec::new(),
            },
        )
    }

    impl AsyncWrite for PipeWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.0.unbounded_send(buf.to_vec()) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.0.close_channel();
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for PipeReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            while self.pending.is_empty() {
                match self.receiver.poll_next_unpin(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(chunk)) => self.pending = chunk,
                    Poll::Ready(None) => return Poll::Ready(Ok(0)),
                }
            }
            let count = self.pending.len().min(buf.len());
            buf[..count].copy_from_slice(&self.pending[..count]);
            self.pending.drain(..count);
            Poll::Ready(Ok(count))
        }
    }

    /// The bridge's side of the channel, driven by the test.
    struct FakeBridge {
        output: PipeWriter,
        input: PipeReader,
        decoder: BridgeFrameDecoder,
        received: std::collections::VecDeque<BridgeFrame>,
    }

    impl FakeBridge {
        fn send(&mut self, frame: BridgeFrame) {
            let mut encoded = Vec::new();
            encode_frame(&frame, &mut encoded);
            self.output
                .0
                .unbounded_send(encoded)
                .expect("the mux is reading");
        }

        async fn receive(&mut self) -> BridgeFrame {
            let mut buffer = [0u8; 1024];
            while self.received.is_empty() {
                let read = self.input.read(&mut buffer).await.expect("read");
                assert_ne!(read, 0, "the mux stopped writing");
                self.received
                    .extend(self.decoder.push(&buffer[..read]).expect("valid frames"));
            }
            self.received.pop_front().expect("a frame")
        }
    }

    /// Runs `test` against a fresh mux. The test owns the handle: it has to
    /// keep it for as long as the mux should run.
    fn run_mux_test<Test, Output>(test: Test)
    where
        Test: FnOnce(FakeBridge, BridgeConnections, BridgeMuxHandle) -> Output,
        Output: std::future::Future<Output = ()>,
    {
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let (bridge_output, mux_input) = pipe();
        let (mux_output, bridge_input) = pipe();
        let (connections, handle) = spawn_bridge_mux(mux_input, mux_output, |future| {
            spawner.spawn(future).expect("spawn the mux");
        });
        let bridge = FakeBridge {
            output: bridge_output,
            input: bridge_input,
            decoder: BridgeFrameDecoder::new(),
            received: Default::default(),
        };
        pool.run_until(test(bridge, connections, handle));
    }

    async fn read_some(connection: &mut BridgeConnection) -> io::Result<Vec<u8>> {
        let mut buffer = vec![0; 256];
        let read = connection.read(&mut buffer).await?;
        buffer.truncate(read);
        Ok(buffer)
    }

    #[test]
    fn a_connection_carries_data_both_ways_until_the_bridge_closes_it() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send(BridgeFrame::open(1));
            let mut connection = connections.next().await.expect("a connection");
            assert_eq!(connection.connection_id(), 1);

            bridge.send(BridgeFrame::data(1, b"hello".to_vec()));
            assert_eq!(read_some(&mut connection).await.expect("read"), b"hello");
            connection.write_all(b"world").await.expect("write");
            assert_eq!(
                bridge.receive().await,
                BridgeFrame::data(1, b"world".to_vec())
            );

            bridge.send(BridgeFrame::close(1));
            assert!(
                read_some(&mut connection)
                    .await
                    .expect("a clean end")
                    .is_empty()
            );
            let error = connection.write_all(b"late").await.expect_err("closed");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        });
    }

    #[test]
    fn closing_or_dropping_a_connection_tells_the_bridge() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send(BridgeFrame::open(4));
            bridge.send(BridgeFrame::open(5));
            let mut closed = connections.next().await.expect("a connection");
            let dropped = connections.next().await.expect("a connection");

            closed.write_all(b"bye").await.expect("write");
            closed.close().await.expect("close");
            assert_eq!(
                bridge.receive().await,
                BridgeFrame::data(4, b"bye".to_vec())
            );
            assert_eq!(bridge.receive().await, BridgeFrame::close(4));
            assert!(
                read_some(&mut closed)
                    .await
                    .expect("reads end with the close")
                    .is_empty()
            );

            drop(dropped);
            assert_eq!(bridge.receive().await, BridgeFrame::close(5));
            // The bridge's own frames for them, crossing on the way, are dropped.
            bridge.send(BridgeFrame::data(5, b"late".to_vec()));
            bridge.send(BridgeFrame::close(5));

            bridge.send(BridgeFrame::open(6));
            let mut later = connections.next().await.expect("the channel still works");
            bridge.send(BridgeFrame::data(6, b"ok".to_vec()));
            assert_eq!(read_some(&mut later).await.expect("read"), b"ok");
        });
    }

    #[test]
    fn several_connections_are_kept_apart() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            for connection_id in [10, 11, 12] {
                bridge.send(BridgeFrame::open(connection_id));
            }
            let mut opened = Vec::new();
            for _ in 0..3 {
                opened.push(connections.next().await.expect("a connection"));
            }
            for connection_id in [12, 10, 11, 10] {
                bridge.send(BridgeFrame::data(
                    connection_id,
                    format!("to {connection_id};").into_bytes(),
                ));
            }
            let mut received = HashMap::new();
            for connection in &mut opened {
                let expected: &[u8] = if connection.connection_id() == 10 {
                    b"to 10;to 10;"
                } else {
                    b"to 1x;"
                };
                let mut bytes = Vec::new();
                while bytes.len() < expected.len() {
                    bytes.extend(read_some(connection).await.expect("read"));
                }
                received.insert(connection.connection_id(), bytes);
            }
            assert_eq!(received[&10], b"to 10;to 10;");
            assert_eq!(received[&11], b"to 11;");
            assert_eq!(received[&12], b"to 12;");

            for connection in &mut opened {
                let id = connection.connection_id();
                connection
                    .write_all(format!("from {id}").as_bytes())
                    .await
                    .expect("write");
            }
            let mut sent = HashMap::new();
            for _ in 0..3 {
                let frame = bridge.receive().await;
                assert_eq!(frame.kind, BridgeFrameKind::Data);
                sent.insert(frame.connection_id, frame.payload);
            }
            for id in [10, 11, 12] {
                assert_eq!(sent[&id], format!("from {id}").into_bytes());
            }
        });
    }

    #[test]
    fn writes_larger_than_a_frame_are_split() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send(BridgeFrame::open(1));
            let mut connection = connections.next().await.expect("a connection");
            let payload: Vec<u8> = (0..200_000).map(|index| (index % 253) as u8).collect();
            connection.write_all(&payload).await.expect("write");
            let mut received = Vec::new();
            while received.len() < payload.len() {
                let frame = bridge.receive().await;
                assert!(frame.payload.len() <= MAX_BRIDGE_FRAME_PAYLOAD);
                received.extend(frame.payload);
            }
            assert_eq!(received, payload);
        });
    }

    #[test]
    fn the_end_of_the_bridge_output_ends_the_channel() {
        run_mux_test(|bridge, mut connections, handle| async move {
            let _mux = handle;
            let FakeBridge {
                mut output, input, ..
            } = bridge;
            let mut encoded = Vec::new();
            encode_frame(&BridgeFrame::open(1), &mut encoded);
            output.0.unbounded_send(encoded).expect("send");
            let mut connection = connections.next().await.expect("a connection");
            output.close().await.expect("close");
            assert!(connections.next().await.is_none(), "no more connections");
            let error = read_some(&mut connection).await.expect_err("aborted");
            assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
            assert!(connection.write_all(b"x").await.is_err());
            drop(input);
        });
    }

    #[test]
    fn a_protocol_error_ends_the_channel() {
        run_mux_test(|bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send_raw(&[9, 0, 0, 0, 0, 0, 0, 0, 0]);
            assert!(connections.next().await.is_none());
        });
    }

    #[test]
    fn opening_a_connection_twice_is_a_protocol_error() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send(BridgeFrame::open(2));
            let mut connection = connections.next().await.expect("a connection");
            bridge.send(BridgeFrame::open(2));
            assert!(connections.next().await.is_none());
            assert!(read_some(&mut connection).await.is_err());
        });
    }

    #[test]
    fn dropping_the_handle_stops_the_mux() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            bridge.send(BridgeFrame::open(1));
            let mut connection = connections.next().await.expect("a connection");
            drop(handle);
            let error = read_some(&mut connection).await.expect_err("aborted");
            assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
            assert!(connections.next().await.is_none());
        });
    }

    #[test]
    fn a_burst_of_small_frames_to_an_unread_connection_is_all_delivered() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send(BridgeFrame::open(1));
            let mut connection = connections.next().await.expect("a connection");
            // Half the budget in full frames, then far more small frames than
            // any count-based limit would take, all before anything is read.
            let mut expected = Vec::new();
            for _ in 0..(INBOUND_BYTES_PER_CONNECTION / MAX_BRIDGE_FRAME_PAYLOAD / 2) {
                let payload = vec![b'a'; MAX_BRIDGE_FRAME_PAYLOAD];
                expected.extend_from_slice(&payload);
                bridge.send(BridgeFrame::data(1, payload));
            }
            for index in 0..500u32 {
                let payload = format!("{index:04}").into_bytes();
                expected.extend_from_slice(&payload);
                bridge.send(BridgeFrame::data(1, payload));
            }
            let mut received = vec![0u8; expected.len()];
            connection
                .read_exact(&mut received)
                .await
                .expect("nothing was dropped");
            assert!(
                received == expected,
                "the bytes arrive complete and in order"
            );
            connection.write_all(b"ok").await.expect("still open");
            assert_eq!(bridge.receive().await, BridgeFrame::data(1, b"ok".to_vec()));
        });
    }

    #[test]
    fn a_stalled_reader_is_closed_and_the_others_keep_working() {
        run_mux_test(|mut bridge, mut connections, handle| async move {
            let _mux = handle;
            bridge.send(BridgeFrame::open(1));
            bridge.send(BridgeFrame::open(2));
            let mut stalled = connections.next().await.expect("a connection");
            let mut working = connections.next().await.expect("a connection");
            assert_eq!(stalled.connection_id(), 1);
            assert_eq!(working.connection_id(), 2);

            // Flood connection 1, which nobody reads: its ~1 MiB inbound buffer
            // fills and the mux closes that one connection.
            for _ in 0..(INBOUND_BYTES_PER_CONNECTION / MAX_BRIDGE_FRAME_PAYLOAD + 8) {
                bridge.send(BridgeFrame::data(1, vec![0u8; MAX_BRIDGE_FRAME_PAYLOAD]));
            }
            // Connection 2 still round-trips — the shared pipe was never
            // blocked by the stalled one.
            bridge.send(BridgeFrame::data(2, b"ping".to_vec()));
            assert_eq!(read_some(&mut working).await.expect("read"), b"ping");
            working.write_all(b"pong").await.expect("write");

            // The stalled connection is closed back to the bridge, and its reads
            // end.
            let mut saw_close = false;
            let mut saw_pong = false;
            while !(saw_close && saw_pong) {
                match bridge.receive().await {
                    frame if frame == BridgeFrame::close(1) => saw_close = true,
                    frame if frame == BridgeFrame::data(2, b"pong".to_vec()) => saw_pong = true,
                    _ => {}
                }
            }
            // The stalled connection's reads end: after whatever was already
            // buffered drains, the abort surfaces.
            let mut buffer = vec![0u8; MAX_BRIDGE_FRAME_PAYLOAD];
            loop {
                match stalled.read(&mut buffer).await {
                    Ok(count) if count > 0 => continue,
                    outcome => {
                        assert!(
                            outcome.is_err(),
                            "the stalled one is closed, not a clean EOF"
                        );
                        break;
                    }
                }
            }
        });
    }

    impl FakeBridge {
        fn send_raw(&self, bytes: &[u8]) {
            self.output
                .0
                .unbounded_send(bytes.to_vec())
                .expect("the mux is reading");
        }
    }
}
