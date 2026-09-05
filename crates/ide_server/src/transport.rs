//! 127.0.0.1 だけに bind する最小のトランスポート。
//!
//! 同じポートで 2 つの話し方を受ける:
//!
//! - **WebSocket** — Claude Code CLI の IDE プロトコル。CLI は
//!   `Sec-WebSocket-Protocol: mcp` と `X-Claude-Code-Ide-Authorization: <token>` を
//!   付けて `ws://127.0.0.1:<port>` に繋いでくる
//! - **streamable HTTP の MCP** — `POST /mcp` に JSON-RPC を 1 件。認証は
//!   `Authorization: Bearer <token>`
//!
//! HTTP/1.1 の要求行とヘッダーだけを自前で読み、WebSocket の 101 応答も自前で書く。
//! フレーミングから先は `async-tungstenite` に渡す (`from_raw_socket`)。新しい重量級の
//! HTTP 依存を足さないための判断。

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::{Message, Role};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt as _};
use smol::net::TcpStream;

/// 要求ヘッダーの上限。これを超えたら切る。
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// `POST /mcp` の本文の上限。
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub const CLAUDE_CODE_AUTH_HEADER: &str = "x-claude-code-ide-authorization";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    WebSocket,
    Http,
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
    pub fn presented_token(&self) -> Option<&str> {
        if let Some(token) = self.header(CLAUDE_CODE_AUTH_HEADER) {
            return Some(token);
        }
        self.header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
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

async fn read_head(stream: &mut TcpStream) -> Result<(HttpHead, Vec<u8>)> {
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

/// 1 本の接続を処理する。受信した JSON-RPC のテキストを `incoming_tx` に流し、
/// `outgoing_rx` から来たテキストを送り返す。接続の種類が決まった時点で `kind_tx`
/// に知らせる (WebSocket のときだけサーバー側から通知を撃てるため)。
pub async fn serve_connection(
    mut stream: TcpStream,
    auth_token: Arc<str>,
    incoming_tx: UnboundedSender<String>,
    outgoing_rx: UnboundedReceiver<String>,
    kind_tx: oneshot::Sender<ConnectionKind>,
) -> Result<()> {
    let (head, leftover) = read_head(&mut stream).await?;

    if head.presented_token() != Some(&*auth_token) {
        let body = "{\"error\":\"unauthorized\"}";
        stream
            .write_all(http_response("401 Unauthorized", Some("application/json"), body).as_bytes())
            .await?;
        stream.flush().await?;
        anyhow::bail!("rejected an IDE server connection with a bad or missing token");
    }

    if head.is_websocket_upgrade() {
        kind_tx.send(ConnectionKind::WebSocket).ok();
        serve_websocket(stream, head, leftover, incoming_tx, outgoing_rx).await
    } else {
        kind_tx.send(ConnectionKind::Http).ok();
        serve_http(stream, head, leftover, incoming_tx, outgoing_rx).await
    }
}

async fn serve_websocket(
    mut stream: TcpStream,
    head: HttpHead,
    leftover: Vec<u8>,
    incoming_tx: UnboundedSender<String>,
    mut outgoing_rx: UnboundedReceiver<String>,
) -> Result<()> {
    anyhow::ensure!(
        leftover.is_empty(),
        "the WebSocket client sent frames before the handshake completed"
    );
    let response = websocket_accept_response(&head)?;
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    let socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    // `async-tungstenite` の固有の `split` は送受信を `Arc` で共有した
    // `WebSocketSender` / `WebSocketReceiver` に分ける (`futures` の `StreamExt::split`
    // ではない)。どちらも自前の `send` / `close` を持つ。
    let (mut sender, mut receiver) = socket.split();

    let write_task = async move {
        while let Some(text) = outgoing_rx.next().await {
            sender.send(Message::text(text)).await?;
        }
        sender.close(None).await?;
        anyhow::Ok(())
    };

    let read_task = async move {
        while let Some(message) = receiver.next().await {
            match message? {
                Message::Text(text) => {
                    if incoming_tx.unbounded_send(text.to_string()).is_err() {
                        break;
                    }
                }
                Message::Binary(bytes) => {
                    let text = String::from_utf8(bytes.to_vec())
                        .context("a binary WebSocket frame was not UTF-8 JSON")?;
                    if incoming_tx.unbounded_send(text).is_err() {
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

async fn serve_http(
    mut stream: TcpStream,
    head: HttpHead,
    leftover: Vec<u8>,
    incoming_tx: UnboundedSender<String>,
    mut outgoing_rx: UnboundedReceiver<String>,
) -> Result<()> {
    let path = head.path.split('?').next().unwrap_or(&head.path);
    if path != "/mcp" {
        stream
            .write_all(http_response("404 Not Found", None, "").as_bytes())
            .await?;
        stream.flush().await?;
        return Ok(());
    }
    if !head.method.eq_ignore_ascii_case("POST") {
        stream
            .write_all(
                http_response(
                    "405 Method Not Allowed",
                    Some("application/json"),
                    "{\"error\":\"only POST /mcp is supported\"}",
                )
                .as_bytes(),
            )
            .await?;
        stream.flush().await?;
        return Ok(());
    }

    let content_length = head
        .header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    anyhow::ensure!(
        content_length <= MAX_BODY_BYTES,
        "the POST /mcp body is too large"
    );
    let mut body = leftover;
    while body.len() < content_length {
        let mut chunk = vec![0u8; content_length - body.len()];
        let read = stream.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "the connection closed before the request body");
        body.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8(body).context("the POST /mcp body was not UTF-8")?;

    incoming_tx
        .unbounded_send(body)
        .map_err(|_| anyhow!("the IDE server stopped before handling the request"))?;
    drop(incoming_tx);

    // 通知だけの POST (`id` 無し) には返す本文が無いので 202 を返す。
    let response = match outgoing_rx.next().await {
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
}
