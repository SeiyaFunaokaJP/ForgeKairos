//! 127.0.0.1 だけに bind する最小のトランスポート。
//!
//! 同じポートで 2 つの話し方を受ける:
//!
//! - **WebSocket** — Claude Code CLI の IDE プロトコル。CLI は
//!   `Sec-WebSocket-Protocol: mcp` と `X-Claude-Code-Ide-Authorization: <token>` を
//!   付けて `ws://127.0.0.1:<port>` に繋いでくる
//! - **streamable HTTP の MCP** — `POST /mcp` に JSON-RPC を 1 件。認証は
//!   `Authorization: Bearer <token>`。`POST /mcp/zed` はアシストのツール (ADR-F56) だけの口
//!
//! HTTP/1.1 の要求行とヘッダーだけを自前で読み、WebSocket の 101 応答も自前で書く。
//! フレーミングから先は `async-tungstenite` に渡す (`from_raw_socket`)。新しい重量級の
//! HTTP 依存を足さないための判断。
//!
//! 受けるのはバイト列なら何でもよい。ホストでは loopback の TCP、サンドボックスの
//! セッション (ADR-F49) ではヘルパーのブリッジが接続ごとに運んでくるストリーム。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::{Message, Role, WebSocketConfig};
use futures::channel::{mpsc, oneshot};
use futures::future::Either;
use futures::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, SinkExt as _, Stream, StreamExt as _,
};
use smol::lock::SemaphoreGuardArc;

/// 要求ヘッダーの上限。これを超えたら切る。
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// ホストの接続で受ける `POST /mcp` の本文と、WebSocket の 1 メッセージの上限。大きな
/// ファイル (ロックファイルなど) の編集提案も `openDiff` の本文で運ばれてくるので、大きめに取る。
pub const HOST_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// 1 メッセージの上限。受け取る側と送る側で別々に置ける。
///
/// 別々に置けるのは、本文の長さを自分で見ている `POST /mcp` (制限セッションが話すのは
/// こちらだけ、ADR-F49 決定 24)。WebSocket の [`WebSocketConfig::max_message_size`] は
/// 送受信の両方に掛かる契約なので、そちらには [`MessageLimits::outgoing`] を渡し、こちらが
/// 返す応答 (`getDiagnostics` はファイル 100 件ぶんになりうる) を切らないようにする。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageLimits {
    /// 相手から受け取る 1 メッセージ (`POST /mcp` なら本文) の上限。
    pub incoming: usize,
    /// こちらから送る 1 メッセージの上限。
    pub outgoing: usize,
}

impl MessageLimits {
    /// 受け取る側も送る側も同じ上限。
    pub fn same(bytes: usize) -> Self {
        Self {
            incoming: bytes,
            outgoing: bytes,
        }
    }
}

/// 要求ヘッダー (HTTP なら本文も) を受け取り終えるまでの猶予。黙って繋ぎっぱなしにする
/// 相手に接続を持たれ続けないため。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 相手が読まないあいだに溜めておく通知の上限。超えた分は捨てる。
pub const MAX_QUEUED_NOTIFICATIONS: usize = 64;

pub const CLAUDE_CODE_AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// streamable HTTP の IDE の口 (ホストでは全ツール、サンドボックスではセッションのツール)。
pub const IDE_MCP_PATH: &str = "/mcp";
/// アシストのツール (ADR-F56) だけを出す口。HTTP でだけ受ける。
pub const ASSIST_MCP_PATH: &str = "/mcp/zed";
/// ADR-F102: the AI browser's tools (ADR-F89) alone, so that an external agent's
/// thread can be given them or not apart from the IDE tools. HTTP only, on the
/// host's server only.
pub const BROWSER_MCP_PATH: &str = "/mcp/zed-browser";

/// 接続を受けてよいトークン。
#[derive(Clone)]
pub enum Credentials {
    /// 1 つのトークンだけ (サンドボックスのセッション)。
    Single(Arc<str>),
    /// ホストのサーバー: サーバー自身のトークン (ロックファイル・`.mcp.json`) と、登録された
    /// アシストのクライアント (CLI のタブ・ACP の接続) ごとのトークン。クライアントは URL の
    /// 申告ではなく、どのトークンを出したかで見分ける (ADR-F56)。
    Host {
        server_token: Arc<str>,
        client_tokens: ClientTokens,
    },
}

/// クライアントごとのトークン → クライアントの ID。登録と解除はフォアグラウンド、照合は
/// トランスポートのバックグラウンドで行う。
pub type ClientTokens = Arc<Mutex<collections::HashMap<String, String>>>;

impl Credentials {
    /// 受けてよいトークンなら `Some` で、クライアントのトークンならその ID を包んで返す
    /// (サーバー自身のトークンは `Some(None)`)。
    pub fn authenticate(&self, presented: &str) -> Option<Option<String>> {
        match self {
            Self::Single(token) => (presented == &**token).then_some(None),
            Self::Host {
                server_token,
                client_tokens,
            } => {
                if presented == &**server_token {
                    return Some(None);
                }
                client_tokens
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(presented)
                    .map(|client_id| Some(client_id.clone()))
            }
        }
    }
}

impl From<Arc<str>> for Credentials {
    fn from(token: Arc<str>) -> Self {
        Self::Single(token)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    WebSocket,
    Http,
}

/// 接続がどの口に来たか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// WebSocket (パスは問わない) と `POST /mcp`。
    Ide,
    /// `POST /mcp/zed`。
    Assist,
    /// `POST /mcp/zed-browser` (ADR-F102)。
    Browser,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub kind: ConnectionKind,
    pub endpoint: Endpoint,
    /// 出されたトークンが登録されたアシストのクライアントのものなら、その ID。ホストの
    /// サーバーが、表示の出どころ (CLI のタブ・ACP の接続) を見分けるのに使う。
    pub client: Option<String>,
}

/// 口ごとの受け入れの判断 ([`serve_connection`] の `admit`) が通したもの。
pub struct Admitted {
    pub limits: MessageLimits,
    /// トランスポートが閉じるまで持っておく (開いている接続の数に入れておくため)。
    pub hold: Option<crate::OpenTransport>,
}

/// 口ごとの受け入れの判断が断った理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// この口は開いていない (設定で無効など)。
    NotFound,
    /// この口の同時接続の上限に達している。
    Busy,
}

/// 要求の頭から、どの口のどの話し方かを決める。口の無いパスは `404`、HTTP の `POST` 以外は
/// `405` の応答を返す。問い合わせ文字列は見ない。
fn route_for(head: &HttpHead, client: Option<String>) -> std::result::Result<Route, String> {
    let path = head
        .path
        .split_once('?')
        .map_or(head.path.as_str(), |(path, _)| path);
    if head.is_websocket_upgrade() {
        // アシストの口は streamable HTTP だけ。WebSocket で来たら、CLI の IDE の口と
        // 取り違えないように断る。
        if path == ASSIST_MCP_PATH || path == BROWSER_MCP_PATH {
            return Err(http_response("404 Not Found", None, ""));
        }
        return Ok(Route {
            kind: ConnectionKind::WebSocket,
            endpoint: Endpoint::Ide,
            client,
        });
    }
    let endpoint = match path {
        IDE_MCP_PATH => Endpoint::Ide,
        ASSIST_MCP_PATH => Endpoint::Assist,
        BROWSER_MCP_PATH => Endpoint::Browser,
        _ => return Err(http_response("404 Not Found", None, "")),
    };
    if !head.method.eq_ignore_ascii_case("POST") {
        return Err(http_response(
            "405 Method Not Allowed",
            Some("application/json"),
            "{\"error\":\"only POST is supported\"}",
        ));
    }
    Ok(Route {
        kind: ConnectionKind::Http,
        endpoint,
        client,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHead {
    pub method: String,
    pub path: String,
    headers: Vec<(String, String)>,
    pub head_len: usize,
}

impl HttpHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        let upgrade = self
            .header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        let connection = self.header("connection").is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
        upgrade && connection
    }

    /// CLI は WebSocket に専用ヘッダー、HTTP MCP は `Authorization: Bearer` を使う。
    ///
    /// RFC 7235 の認証スキームは大文字小文字を区別せず、スキームと値のあいだの空白も 1 つ
    /// とは限らない。MCP のクライアントは `bearer <token>` を出すものもあるので、そのまま
    /// 401 にしない (トークン自体の比較は呼び出し側で、こちらは完全一致)。
    pub fn presented_token(&self) -> Option<&str> {
        if let Some(token) = self.header(CLAUDE_CODE_AUTH_HEADER) {
            return Some(token);
        }
        let (scheme, token) = self.header("authorization")?.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        Some(token.trim())
    }
}

/// ヘッダーが揃っていなければ `Ok(None)` を返す (呼び出し側がもっと読む)。
pub fn parse_head(buffer: &[u8]) -> Result<Option<HttpHead>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    let status = request
        .parse(buffer)
        .context("parsing the HTTP request head")?;
    let head_len = match status {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => return Ok(None),
    };
    let method = request
        .method
        .ok_or_else(|| anyhow!("the HTTP request has no method"))?
        .to_string();
    let path = request
        .path
        .ok_or_else(|| anyhow!("the HTTP request has no path"))?
        .to_string();
    let headers = request
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_ascii_lowercase(),
                String::from_utf8_lossy(header.value).trim().to_string(),
            )
        })
        .collect();
    Ok(Some(HttpHead {
        method,
        path,
        headers,
        head_len,
    }))
}

/// RFC 6455 の 101 応答。要求された副プロトコルに `mcp` があればそれを返す。
pub fn websocket_accept_response(head: &HttpHead) -> Result<String> {
    let key = head
        .header("sec-websocket-key")
        .ok_or_else(|| anyhow!("the WebSocket upgrade has no Sec-WebSocket-Key"))?;
    let accept = derive_accept_key(key.as_bytes());
    let mut response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n"
    );
    let requested_mcp = head
        .header("sec-websocket-protocol")
        .is_some_and(|value| value.split(',').any(|name| name.trim() == "mcp"));
    if requested_mcp {
        response.push_str("Sec-WebSocket-Protocol: mcp\r\n");
    }
    response.push_str("\r\n");
    Ok(response)
}

pub fn http_response(status_line: &str, content_type: Option<&str>, body: &str) -> String {
    let mut response = format!("HTTP/1.1 {status_line}\r\n");
    if let Some(content_type) = content_type {
        response.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    response.push_str("Connection: close\r\n\r\n");
    response.push_str(body);
    response
}

/// Zed から 1 本の接続へ送るもの。応答は必ず届け、通知は相手が読まずに溜まりすぎたら
/// 捨て、`selection_changed` は最新の 1 件だけを残す。読まない相手のせいで Zed の
/// メモリが増え続けないようにするため。
pub struct Outbox {
    state: Mutex<OutboxState>,
}

#[derive(Default)]
struct OutboxState {
    /// 応答と、その要求が握っていた同時実行の枠。枠は送り出した時点で返す。
    responses: VecDeque<(String, Option<SemaphoreGuardArc>)>,
    notifications: VecDeque<String>,
    selection: Option<String>,
    finished: bool,
    receiver_dropped: bool,
    waker: Option<Waker>,
}

pub fn outbox() -> (Arc<Outbox>, OutboxReceiver) {
    let outbox = Arc::new(Outbox {
        state: Mutex::new(OutboxState::default()),
    });
    (outbox.clone(), OutboxReceiver { outbox })
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, OutboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn send_response(&self, text: String, permit: Option<SemaphoreGuardArc>) {
        let waker = {
            let mut state = self.lock();
            if state.receiver_dropped {
                return;
            }
            state.responses.push_back((text, permit));
            state.waker.take()
        };
        wake(waker);
    }

    pub fn send_notification(&self, text: String) -> Delivery {
        let waker = {
            let mut state = self.lock();
            if state.receiver_dropped || state.finished {
                return Delivery::Closed;
            }
            if state.notifications.len() >= MAX_QUEUED_NOTIFICATIONS {
                log::debug!("ide_server: dropped a notification for a client that is not reading");
                return Delivery::Dropped;
            }
            state.notifications.push_back(text);
            state.waker.take()
        };
        wake(waker);
        Delivery::Queued
    }

    /// まだ送っていない `selection_changed` を最新のものに置き換える。
    pub fn send_selection(&self, text: String) -> Delivery {
        let waker = {
            let mut state = self.lock();
            if state.receiver_dropped || state.finished {
                return Delivery::Closed;
            }
            state.selection = Some(text);
            state.waker.take()
        };
        wake(waker);
        Delivery::Queued
    }

    /// これ以上は送らない。溜まっている分を送り切ったら受け手の側が終わる。
    pub fn finish(&self) {
        let waker = {
            let mut state = self.lock();
            state.finished = true;
            state.waker.take()
        };
        wake(waker);
    }

    pub fn is_closed(&self) -> bool {
        let state = self.lock();
        state.receiver_dropped || state.finished
    }
}

/// 通知を積めたかどうか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Queued,
    /// 相手が読まずに溜まっているので捨てた。接続は生きている。
    Dropped,
    /// 相手がもう居ない。
    Closed,
}

fn wake(waker: Option<Waker>) {
    if let Some(waker) = waker {
        waker.wake();
    }
}

pub struct OutboxReceiver {
    outbox: Arc<Outbox>,
}

impl Stream for OutboxReceiver {
    type Item = String;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<String>> {
        let mut state = self.outbox.lock();
        if let Some((text, permit)) = state.responses.pop_front() {
            drop(state);
            drop(permit);
            return Poll::Ready(Some(text));
        }
        if let Some(text) = state.selection.take() {
            return Poll::Ready(Some(text));
        }
        if let Some(text) = state.notifications.pop_front() {
            return Poll::Ready(Some(text));
        }
        if state.finished {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for OutboxReceiver {
    fn drop(&mut self) {
        // 捨てる応答が握っている同時実行の枠は、ここで返さないとセッションが詰まる。
        let responses = {
            let mut state = self.outbox.lock();
            state.receiver_dropped = true;
            state.notifications.clear();
            state.selection = None;
            std::mem::take(&mut state.responses)
        };
        drop(responses);
    }
}

async fn before_deadline<T>(
    future: impl Future<Output = T>,
    deadline: Pin<&mut impl Future<Output = ()>>,
) -> Result<T> {
    futures::pin_mut!(future);
    match futures::future::select(future, deadline).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(((), _)) => Err(anyhow!("the client did not send its request in time")),
    }
}

async fn read_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    mut body: Vec<u8>,
    content_length: usize,
) -> Result<Vec<u8>> {
    if body.len() < content_length {
        let remaining = u64::try_from(content_length - body.len())?;
        (&mut *stream)
            .take(remaining)
            .read_to_end(&mut body)
            .await?;
    }
    anyhow::ensure!(
        body.len() >= content_length,
        "the connection closed before the request body"
    );
    body.truncate(content_length);
    Ok(body)
}

fn websocket_config(max_message_bytes: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(max_message_bytes))
        .max_frame_size(Some(max_message_bytes))
}

async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(HttpHead, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(head) = parse_head(&buffer)? {
            let rest = buffer.split_off(head.head_len);
            return Ok((head, rest));
        }
        anyhow::ensure!(
            buffer.len() < MAX_HEAD_BYTES,
            "the HTTP request head is too large"
        );
        let read = stream.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "the connection closed before the request head");
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// 1 本の接続を処理する。受信した JSON-RPC のテキストを `incoming_tx` に流し (受け手が
/// 詰まっていれば、相手からの読み込みを止めて待たせる)、`outgoing` から来たテキストを
/// 送り返す。どの口のどの話し方かが決まり、`admit` が受け入れた時点で `route_tx` に知らせる
/// (WebSocket のときだけサーバー側から通知を撃てるため、また口ごとに出すツールが違うため)。
/// `handshake_deadline` までに要求を受け取り終えなければ切る。
///
/// `admit` は口ごとの同時接続の枠とメッセージの上限を決める。要求の頭を読むまでどの口かは
/// 分からないので、枠はここで (認証のあとで) 取る。
#[allow(
    clippy::too_many_arguments,
    reason = "the channels to and from the dispatcher, each used exactly once"
)]
pub async fn serve_connection<S, A>(
    mut stream: S,
    credentials: impl Into<Credentials>,
    admit: A,
    incoming_tx: mpsc::Sender<String>,
    outgoing: OutboxReceiver,
    route_tx: oneshot::Sender<Route>,
    handshake_deadline: impl Future<Output = ()>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    A: FnOnce(&Route) -> std::result::Result<Admitted, Refusal>,
{
    futures::pin_mut!(handshake_deadline);
    let (head, leftover) =
        before_deadline(read_head(&mut stream), handshake_deadline.as_mut()).await??;

    let credentials = credentials.into();
    let Some(client) = head
        .presented_token()
        .and_then(|token| credentials.authenticate(token))
    else {
        let body = "{\"error\":\"unauthorized\"}";
        stream
            .write_all(http_response("401 Unauthorized", Some("application/json"), body).as_bytes())
            .await?;
        stream.flush().await?;
        anyhow::bail!("rejected an IDE server connection with a bad or missing token");
    };

    let route = match route_for(&head, client) {
        Ok(route) => route,
        Err(response) => {
            stream.write_all(response.as_bytes()).await?;
            stream.flush().await?;
            return Ok(());
        }
    };
    let admitted = match admit(&route) {
        Ok(admitted) => admitted,
        Err(refusal) => {
            let response = match refusal {
                Refusal::NotFound => http_response("404 Not Found", None, ""),
                Refusal::Busy => http_response(
                    "503 Service Unavailable",
                    Some("application/json"),
                    "{\"error\":\"too many connections\"}",
                ),
            };
            stream.write_all(response.as_bytes()).await?;
            stream.flush().await?;
            anyhow::bail!("refused an IDE server connection: {refusal:?}");
        }
    };
    let _hold = admitted.hold;
    let limits = admitted.limits;
    let kind = route.kind;
    route_tx.send(route).ok();

    match kind {
        ConnectionKind::WebSocket => {
            serve_websocket(
                stream,
                head,
                leftover,
                limits.outgoing,
                incoming_tx,
                outgoing,
            )
            .await
        }
        ConnectionKind::Http => {
            serve_http(
                stream,
                head,
                leftover,
                limits.incoming,
                incoming_tx,
                outgoing,
                handshake_deadline,
            )
            .await
        }
    }
}

async fn serve_websocket<S>(
    mut stream: S,
    head: HttpHead,
    leftover: Vec<u8>,
    max_message_bytes: usize,
    mut incoming_tx: mpsc::Sender<String>,
    mut outgoing: OutboxReceiver,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    anyhow::ensure!(
        leftover.is_empty(),
        "the WebSocket client sent frames before the handshake completed"
    );
    let response = websocket_accept_response(&head)?;
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    let socket = WebSocketStream::from_raw_socket(
        stream,
        Role::Server,
        Some(websocket_config(max_message_bytes)),
    )
    .await;
    // `async-tungstenite` の固有の `split` は送受信を `Arc` で共有した
    // `WebSocketSender` / `WebSocketReceiver` に分ける (`futures` の `StreamExt::split`
    // ではない)。どちらも自前の `send` / `close` を持つ。
    let (mut sender, mut receiver) = socket.split();

    let write_task = async move {
        while let Some(text) = outgoing.next().await {
            sender.send(Message::text(text)).await?;
        }
        sender.close(None).await?;
        anyhow::Ok(())
    };

    let read_task = async move {
        while let Some(message) = receiver.next().await {
            match message? {
                Message::Text(text) => {
                    if incoming_tx.send(text.to_string()).await.is_err() {
                        break;
                    }
                }
                Message::Binary(bytes) => {
                    let text = String::from_utf8(bytes.to_vec())
                        .context("a binary WebSocket frame was not UTF-8 JSON")?;
                    if incoming_tx.send(text).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        anyhow::Ok(())
    };

    futures::pin_mut!(write_task);
    futures::pin_mut!(read_task);
    match futures::future::select(write_task, read_task).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right((result, _)) => result,
    }
}

async fn serve_http<S, D>(
    mut stream: S,
    head: HttpHead,
    leftover: Vec<u8>,
    max_incoming_bytes: usize,
    mut incoming_tx: mpsc::Sender<String>,
    mut outgoing: OutboxReceiver,
    deadline: Pin<&mut D>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    D: Future<Output = ()>,
{
    let content_length = head
        .header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    anyhow::ensure!(
        content_length <= max_incoming_bytes,
        "the POST /mcp body is too large"
    );
    let body =
        before_deadline(read_body(&mut stream, leftover, content_length), deadline).await??;
    let body = String::from_utf8(body).context("the POST /mcp body was not UTF-8")?;

    incoming_tx
        .send(body)
        .await
        .map_err(|_| anyhow!("the IDE server stopped before handling the request"))?;
    drop(incoming_tx);

    // 通知だけの POST (`id` 無し) には返す本文が無いので 202 を返す。
    let response = match outgoing.next().await {
        Some(text) => http_response("200 OK", Some("application/json"), &text),
        None => http_response("202 Accepted", None, ""),
    };
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(raw: &str) -> HttpHead {
        parse_head(raw.as_bytes())
            .expect("the head should parse")
            .expect("the head should be complete")
    }

    #[test]
    fn parses_a_partial_head_as_incomplete() {
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n").expect("no parse error"),
            None
        );
    }

    #[test]
    fn recognizes_the_claude_code_upgrade() {
        let head = head(
            "GET / HTTP/1.1\r\n\
             Host: 127.0.0.1:1234\r\n\
             Upgrade: websocket\r\n\
             Connection: keep-alive, Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: mcp\r\n\
             X-Claude-Code-Ide-Authorization: secret-token\r\n\r\n",
        );
        assert!(head.is_websocket_upgrade());
        assert_eq!(head.presented_token(), Some("secret-token"));

        let response = websocket_accept_response(&head).expect("the response should build");
        // RFC 6455 の例の鍵に対する既知の値。
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        assert!(response.contains("Sec-WebSocket-Protocol: mcp"));
        assert!(response.ends_with("\r\n\r\n"));
    }

    #[test]
    fn reads_a_bearer_token_for_http_mcp() {
        let head = head(
            "POST /mcp HTTP/1.1\r\n\
             Host: 127.0.0.1:1234\r\n\
             Authorization: Bearer secret-token\r\n\
             Content-Length: 2\r\n\r\n{}",
        );
        assert!(!head.is_websocket_upgrade());
        assert_eq!(head.presented_token(), Some("secret-token"));
        assert_eq!(head.method, "POST");
        assert_eq!(head.path, "/mcp");
    }

    #[test]
    fn ignores_a_token_in_the_wrong_place() {
        let head = head(
            "POST /mcp HTTP/1.1\r\n\
             Host: 127.0.0.1:1234\r\n\
             Authorization: secret-token\r\n\r\n",
        );
        assert_eq!(head.presented_token(), None);
    }

    /// RFC 7235 のスキームは大文字小文字を区別せず、値との間は空白 1 つとは限らない。
    #[test]
    fn reads_a_bearer_token_however_the_client_spells_the_scheme() {
        for authorization in [
            "Bearer secret-token",
            "bearer secret-token",
            "BEARER secret-token",
            "BeArEr secret-token",
            "Bearer  secret-token",
            "bearer \tsecret-token",
            "Bearer secret-token   ",
        ] {
            let head = head(&format!(
                "POST /mcp HTTP/1.1\r\n\
                 Host: 127.0.0.1:1234\r\n\
                 Authorization: {authorization}\r\n\r\n"
            ));
            assert_eq!(
                head.presented_token(),
                Some("secret-token"),
                "{authorization:?}"
            );
        }

        for authorization in ["Basic secret-token", "Bearer", "bearertoken"] {
            let head = head(&format!(
                "POST /mcp HTTP/1.1\r\n\
                 Host: 127.0.0.1:1234\r\n\
                 Authorization: {authorization}\r\n\r\n"
            ));
            assert_eq!(head.presented_token(), None, "{authorization:?}");
        }
    }

    /// ADR-F56: `/mcp` は IDE の口、`/mcp/zed` はアシストの口。どちらも HTTP は `POST` だけで、
    /// アシストの口は WebSocket では受けない。WebSocket の IDE の口はパスを問わない (CLI は `/`)。
    #[test]
    fn routes_requests_to_the_ide_and_assist_endpoints() {
        let route = |raw: &str| route_for(&head(raw), None);
        let post = |path: &str| {
            route(&format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n"
            ))
        };

        let ide = post("/mcp").expect("POST /mcp is the IDE endpoint");
        assert_eq!(ide.endpoint, Endpoint::Ide);
        assert_eq!(ide.kind, ConnectionKind::Http);
        assert_eq!(ide.client, None);

        let assist = post("/mcp/zed?client=abc-123_x").expect("POST /mcp/zed is the assist one");
        assert_eq!(assist.endpoint, Endpoint::Assist);
        assert_eq!(
            assist.client, None,
            "a client is known by its token, not by what the URL says"
        );

        for refused in ["/", "/mcp/", "/mcp/zed/", "/mcp/other", "/mcpzed"] {
            let response = post(refused).expect_err(refused);
            assert!(
                response.starts_with("HTTP/1.1 404"),
                "{refused}: {response}"
            );
        }
        let response = route("GET /mcp/zed HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .expect_err("only POST is taken");
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");

        let upgrade = |path: &str| {
            route(&format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: 127.0.0.1\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            ))
        };
        let websocket = upgrade("/").expect("the CLI's WebSocket");
        assert_eq!(websocket.kind, ConnectionKind::WebSocket);
        assert_eq!(websocket.endpoint, Endpoint::Ide);
        assert!(
            upgrade("/mcp/zed")
                .expect_err("no WebSocket on the assist endpoint")
                .starts_with("HTTP/1.1 404")
        );

        // ADR-F102: the AI browser's own endpoint, HTTP only.
        let browser = post("/mcp/zed-browser").expect("POST /mcp/zed-browser is the browser's");
        assert_eq!(browser.endpoint, Endpoint::Browser);
        assert!(
            upgrade("/mcp/zed-browser")
                .expect_err("no WebSocket on the browser's endpoint")
                .starts_with("HTTP/1.1 404")
        );
    }

    /// ADR-F56: ホストのサーバーはサーバー自身のトークンと、登録されたクライアントごとの
    /// トークンを受け、クライアントのトークンからそのクライアントを見分ける。登録を外した
    /// トークンは受けない。
    #[test]
    fn host_credentials_tell_the_clients_apart_by_their_tokens() {
        let client_tokens = ClientTokens::default();
        let credentials = Credentials::Host {
            server_token: "server-token".into(),
            client_tokens: client_tokens.clone(),
        };
        assert_eq!(credentials.authenticate("server-token"), Some(None));
        assert_eq!(credentials.authenticate("client-token"), None);

        client_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert("client-token".to_string(), "client-a".to_string());
        assert_eq!(
            credentials.authenticate("client-token"),
            Some(Some("client-a".to_string()))
        );
        assert_eq!(
            credentials.authenticate("client-a"),
            None,
            "the id is no token"
        );
        assert_eq!(credentials.authenticate(""), None);

        client_tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove("client-token");
        assert_eq!(credentials.authenticate("client-token"), None);

        let single = Credentials::from(Arc::<str>::from("session-token"));
        assert_eq!(single.authenticate("session-token"), Some(None));
        assert_eq!(single.authenticate("server-token"), None);
    }

    #[test]
    fn outbox_keeps_responses_caps_notifications_and_coalesces_selections() {
        let (outbox, receiver) = outbox();
        for index in 0..MAX_QUEUED_NOTIFICATIONS {
            assert_eq!(
                outbox.send_notification(format!("notification {index}")),
                Delivery::Queued
            );
        }
        assert_eq!(
            outbox.send_notification("one too many".to_string()),
            Delivery::Dropped,
            "a dropped notification is not reported as sent"
        );
        assert_eq!(
            outbox.send_selection("old selection".to_string()),
            Delivery::Queued
        );
        assert_eq!(
            outbox.send_selection("new selection".to_string()),
            Delivery::Queued
        );
        outbox.send_response("response".to_string(), None);
        outbox.finish();

        let received = smol::block_on(receiver.collect::<Vec<_>>());
        assert_eq!(
            received.first().map(String::as_str),
            Some("response"),
            "responses are never dropped and go first"
        );
        assert_eq!(
            received.get(1).map(String::as_str),
            Some("new selection"),
            "only the latest selection is kept"
        );
        assert_eq!(received.len(), 2 + MAX_QUEUED_NOTIFICATIONS);
    }

    #[test]
    fn dropping_the_receiver_returns_the_request_permits() {
        let permits = Arc::new(smol::lock::Semaphore::new(1));
        let (outbox, receiver) = outbox();
        outbox.send_response("response".to_string(), permits.try_acquire_arc());
        assert!(
            permits.try_acquire_arc().is_none(),
            "the queued response holds the only permit"
        );

        drop(receiver);
        assert!(
            permits.try_acquire_arc().is_some(),
            "a closed connection must not keep the session's permits"
        );
        assert_eq!(
            outbox.send_notification("too late".to_string()),
            Delivery::Closed
        );
        assert!(outbox.is_closed());
    }
}
