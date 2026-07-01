//! A minimal, hand-rolled MCP **HTTP+SSE client transport** (the legacy "2024-11-05" remote
//! transport). rmcp 1.7.0 — and 2.0.0, whose transport feature set is byte-for-byte identical —
//! ships no standalone SSE client: only `client-side-sse`, the SSE *parser* used internally by the
//! streamable-HTTP client. Old SSE-only MCP servers therefore can't be reached via the
//! streamable-HTTP client. This module implements just enough of the SSE spec to drive them, as an
//! [`rmcp::transport::Transport`], so `--transport sse` connects for real instead of collapsing
//! into streamable-HTTP.
//!
//! Wire protocol (per the MCP HTTP+SSE transport):
//!   1. Client opens `GET <url>` with `Accept: text/event-stream`.
//!   2. The server's first event is `event: endpoint`, `data: <url>` — the URL to POST client
//!      messages to (often a relative path with a session query string). Resolved against the GET
//!      URL.
//!   3. Server→client JSON-RPC arrives as further SSE events (`event: message`, or no event name).
//!   4. Client→server JSON-RPC is `POST`ed to the endpoint URL as `application/json`.

use futures::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::{mpsc, watch};

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid url: {0}")]
    Url(String),
    #[error("sse endpoint never arrived")]
    NoEndpoint,
    #[error("serialize: {0}")]
    Serialize(String),
}

/// One parsed SSE event block.
struct SseFrame {
    event: Option<String>,
    data: String,
}

/// Parse a single SSE event block (the text between two blank-line boundaries). Multiple `data:`
/// lines are joined with `\n` per the spec; `event:` sets the type; comments (`:`-prefixed) and
/// other fields are ignored. `block.lines()` tolerates both `\n` and `\r\n` line endings.
fn parse_sse_frame(block: &str) -> SseFrame {
    let mut event = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in block.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }
    SseFrame {
        event,
        data: data_lines.join("\n"),
    }
}

/// Drain the next complete event (everything up to and including the first blank-line terminator)
/// from a streaming buffer, leaving any partial trailing event behind. `\r` is stripped on
/// ingestion, so the only terminator we look for is `\n\n`.
fn take_event(buf: &mut String) -> Option<String> {
    buf.find("\n\n").map(|pos| buf.drain(..pos + 2).collect())
}

/// The background reader: consumes the SSE byte stream, resolves the POST endpoint from the first
/// `endpoint` event, and forwards every subsequent JSON-RPC message to `msg_tx`.
async fn run_reader(
    resp: reqwest::Response,
    base: reqwest::Url,
    endpoint_tx: watch::Sender<Option<reqwest::Url>>,
    msg_tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>>,
) {
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        // Append lossily and strip CR so framing/field parsing only reasons about `\n`. SSE
        // payloads are UTF-8; a chunk may split a multibyte char, but `from_utf8_lossy` keeps the
        // stream moving and the next chunk completes it.
        let text = String::from_utf8_lossy(&bytes);
        buf.extend(text.chars().filter(|&c| c != '\r'));
        while let Some(raw) = take_event(&mut buf) {
            let frame = parse_sse_frame(&raw);
            match frame.event.as_deref() {
                Some("endpoint") => {
                    if let Ok(url) = base.join(frame.data.trim()) {
                        let _ = endpoint_tx.send(Some(url));
                    }
                }
                // Default event type is "message"; a missing event name is also a message.
                _ => {
                    if frame.data.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(&frame.data) {
                        Ok(msg) => {
                            if msg_tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!("sse: dropping unparseable message: {e}"),
                    }
                }
            }
        }
    }
}

/// A connected SSE client transport: a `GET` event-stream for server→client messages plus a
/// resolved `POST` endpoint for client→server messages. Satisfies `Transport<RoleClient>` so it
/// plugs straight into `handler.serve(transport)` like the stdio / streamable-HTTP transports.
pub struct SseClientTransport {
    http: reqwest::Client,
    endpoint_rx: watch::Receiver<Option<reqwest::Url>>,
    msg_rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
    bearer: Option<String>,
    reader: tokio::task::JoinHandle<()>,
}

impl SseClientTransport {
    /// Open the SSE stream and start the background reader. The POST endpoint is resolved
    /// asynchronously from the server's first `endpoint` event; [`Transport::send`] waits for it.
    pub async fn connect(
        client: reqwest::Client,
        url: &str,
        bearer: Option<String>,
    ) -> Result<Self, SseError> {
        let base = reqwest::Url::parse(url).map_err(|e| SseError::Url(e.to_string()))?;
        let mut req = client.get(base.clone()).header(ACCEPT, "text/event-stream");
        if let Some(b) = &bearer {
            req = req.bearer_auth(b);
        }
        let resp = req.send().await?.error_for_status()?;

        let (endpoint_tx, endpoint_rx) = watch::channel(None);
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(run_reader(resp, base, endpoint_tx, msg_tx));
        Ok(Self {
            http: client,
            endpoint_rx,
            msg_rx,
            bearer,
            reader,
        })
    }
}

impl Drop for SseClientTransport {
    /// Abort the background reader even if the transport is dropped without an explicit
    /// `close()` call (e.g. a `connect_timeout` cancellation mid-`initialize`, before this
    /// transport is ever wrapped in a `RunningService`). Without this, `run_reader`'s
    /// `bytes_stream()` — an open HTTP connection to the (possibly untrusted) remote server —
    /// leaks for the life of the process.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Transport<RoleClient> for SseClientTransport {
    type Error = SseError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let http = self.http.clone();
        let mut endpoint_rx = self.endpoint_rx.clone();
        let bearer = self.bearer.clone();
        async move {
            // Block until the server's `endpoint` event arrives, then POST there. The borrow is
            // dropped at the end of each statement, so nothing non-`Send` is held across the await.
            let url = loop {
                let current = endpoint_rx.borrow().clone();
                if let Some(u) = current {
                    break u;
                }
                if endpoint_rx.changed().await.is_err() {
                    return Err(SseError::NoEndpoint);
                }
            };
            let body =
                serde_json::to_string(&item).map_err(|e| SseError::Serialize(e.to_string()))?;
            let mut req = http
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .body(body);
            if let Some(b) = &bearer {
                req = req.bearer_auth(b);
            }
            req.send().await?.error_for_status()?;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.msg_rx.recv().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reader.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_frame() {
        let f = parse_sse_frame("event: endpoint\ndata: /messages?sessionId=abc123\n");
        assert_eq!(f.event.as_deref(), Some("endpoint"));
        assert_eq!(f.data, "/messages?sessionId=abc123");
    }

    #[test]
    fn parses_message_frame_with_default_event() {
        // No `event:` line → default "message". JSON payload preserved verbatim.
        let f = parse_sse_frame("data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
        assert!(f.event.is_none());
        assert_eq!(f.data, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}");
    }

    #[test]
    fn joins_multiline_data_and_ignores_comments() {
        let f = parse_sse_frame(": keep-alive comment\nevent: message\ndata: line1\ndata: line2\n");
        assert_eq!(f.event.as_deref(), Some("message"));
        assert_eq!(f.data, "line1\nline2");
    }

    #[test]
    fn take_event_splits_on_blank_line_and_keeps_partial() {
        let mut buf =
            String::from("event: endpoint\ndata: /m\n\nevent: message\ndata: {\"x\":1}\n\npart");
        let first = take_event(&mut buf).unwrap();
        assert!(first.contains("endpoint"));
        let second = take_event(&mut buf).unwrap();
        assert!(second.contains("message"));
        // The trailing incomplete event (no terminator yet) stays buffered.
        assert!(take_event(&mut buf).is_none());
        assert_eq!(buf, "part");
    }

    #[test]
    fn take_event_collapses_crlf_after_cr_stripping() {
        // Mirrors how the reader stores bytes: CRs already removed, so CRLF framing collapses to a
        // plain `\n\n` terminator.
        let mut buf = String::from("data: hello\n\n");
        let ev = take_event(&mut buf).unwrap();
        assert_eq!(parse_sse_frame(&ev).data, "hello");
        assert!(buf.is_empty());
    }
}
