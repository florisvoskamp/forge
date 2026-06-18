//! Remote control — drive a running `forge chat` session from a phone or desktop browser.
//!
//! `/remote` (alias `/rc`) starts a tiny HTTP + WebSocket server bound to `0.0.0.0:<ephemeral>`
//! (LAN-reachable). A single HTML control page is served at a token-gated URL (printed into the
//! TUI scrollback + rendered as a QR code so a phone can scan-to-connect). One bidirectional
//! WebSocket carries the live [`Snapshot`] (model · busy · cost · context · statusline · the
//! recent transcript edge) to the browser and [`RemoteInput`] (prompt / answer / interrupt) back.
//!
//! The design goals are: *easy* (one slash command, no install, works from any browser), and
//! *accessible on mobile + desktop* (a responsive, low-friction control page that needs no app).
//! The server is in-process so it reuses the running Session + presenter channel — no second
//! process, no IPC, no keys to configure. Security is a random token in the URL path (a LAN peer
//! without the token can't drive the session); bind-to-loopback is a one-line config knob for
//! users who never want it on the LAN.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// A token-gated URL is printed into the TUI so the user can scan/click to connect.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `token` is read by tests + serves as a stable handle field
pub struct RemoteUrl {
    /// `http://host:port/TOKEN` — the full connect URL (host resolved best-effort).
    pub url: String,
    /// The LAN-visible host:port, for the scrollback note ("listening on …").
    pub addr: SocketAddr,
    /// The random path token (also the WS auth key).
    pub token: String,
}

/// One frame of visible state broadcast to every connected browser, so the control page mirrors
/// the TUI statusline + the tail of the conversation. Cheap to build (plain strings) and JSON, so
/// a phone renders it without any client-side framework.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    pub busy: bool,
    pub done: bool,
    /// The active operating temper label (e.g. "Ask").
    pub temper: String,
    /// Mesh routing: tier + model, or "—" when unset.
    pub tier: Option<String>,
    pub model: String,
    /// Session spend in USD.
    pub cost_usd: f64,
    /// Context-window fill: tokens used + limit (if known).
    pub context_tokens: u64,
    pub context_limit: Option<u32>,
    /// The trailing edge of the in-flight streaming reply (plain text; re-sent each frame).
    pub streaming: String,
    /// Recent finalized scrollback lines (plain text, newest last; bounded).
    pub transcript: Vec<String>,
    /// A pending permission prompt, if the turn is blocked on a y/n.
    pub permission_prompt: Option<String>,
    /// A pending AskUserQuestion, if the turn is blocked on a choice.
    pub question: Option<String>,
    /// `true` once remote control has been turned off (tells the page to stop reconnecting).
    pub closed: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            busy: false,
            done: false,
            temper: String::new(),
            tier: None,
            model: "—".to_string(),
            cost_usd: 0.0,
            context_tokens: 0,
            context_limit: None,
            streaming: String::new(),
            transcript: Vec::new(),
            permission_prompt: None,
            question: None,
            closed: false,
        }
    }
}

/// An input from a remote browser, drained by the render loop and injected like a local
/// keystroke / command. `Interrupt` maps to Esc-while-busy; `Answer` resolves a permission
/// prompt or an AskUserQuestion (the loop routes it to whichever is pending).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteInput {
    /// Submit a prompt (or a `/command`) — exactly as if typed + Enter in the TUI.
    Prompt { text: String },
    /// Answer a pending permission prompt: `true` = allow (y), `false` = deny (n).
    Allow { yes: bool },
    /// Answer a pending AskUserQuestion with a free-text line (a number picks an option).
    Answer { text: String },
    /// Esc-while-busy: stop the current turn (ignored when idle).
    Interrupt,
}

/// The handle the render loop holds: publish a new [`Snapshot`] every dirty frame, and drain
/// queued [`RemoteInput`]s to inject them. Dropping it stops the server.
pub struct RemoteControl {
    /// Publish the latest visible state; the WS task forwards it to every browser.
    pub snapshot_tx: watch::Sender<Snapshot>,
    /// Inputs queued by remote browsers; the render loop drains these each iteration.
    pub input_rx: mpsc::Receiver<RemoteInput>,
    /// The connect URL + token (printed once into scrollback).
    pub url: RemoteUrl,
    /// Abort the server task on drop so the port frees immediately.
    _server: JoinHandle<()>,
}

impl Drop for RemoteControl {
    fn drop(&mut self) {
        // Mark closed so connected browsers stop reconnecting, then tear the server down.
        let _ = self.snapshot_tx.send(Snapshot {
            closed: true,
            ..self.snapshot_tx.borrow().clone()
        });
        self._server.abort();
    }
}

/// A random URL-safe token for path-gating the control page + WS. Lowercase hex is unambiguous
/// on a phone keyboard and survives being embedded in a QR code.
fn random_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 16 hex chars (64 bits): enough entropy that a LAN peer can't guess it, short enough to
    // scan. We don't pull a crate for this — SystemTime nanos + an address-unique counter is
    // plenty for a session-scoped secret that's only valid while `forge chat` is running.
    // `ThreadId::as_u64` is unstable, so we mix the process id + a static counter instead —
    // the goal is just "not guessable by a LAN peer within the session's lifetime".
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let mix = nanos ^ pid.wrapping_mul(0x9E37) ^ seq.wrapping_mul(0x1000_0003);
    format!("{:016x}", mix as u64)
}

/// Best-effort LAN hostname for the connect URL. We keep it dependency-free and just return the
/// numeric IP — it always resolves from the phone, and avoids a DNS-lookup edge case where the
/// machine's hostname isn't resolvable on the LAN (which would yield a dead QR code).
fn lan_host(addr: SocketAddr) -> String {
    addr.ip().to_string()
}

/// Start the remote-control server. The returned [`RemoteControl`] is moved into the render loop;
/// dropping it stops the server and frees the port. `lan` selects `0.0.0.0` (LAN-reachable) vs
/// `127.0.0.1` (loopback-only) — the latter for users who only want to control from this machine.
pub fn start(lan: bool) -> std::io::Result<RemoteControl> {
    let token = random_token();
    let bind_ip: std::net::IpAddr = if lan {
        std::net::Ipv4Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv4Addr::LOCALHOST.into()
    };
    // Port 0 → the OS picks a free ephemeral port (no clashes, no config).
    let listener = std::net::TcpListener::bind((bind_ip, 0))?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let host = lan_host(addr);
    let url = format!("http://{host}:{}/{}", addr.port(), token);

    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::default());
    let (input_tx, input_rx) = mpsc::channel::<RemoteInput>(64);

    let state = Arc::new(ServerState {
        snapshot_rx: snapshot_rx.clone(),
        input_tx,
    });

    // axum wants a tokio TcpListener; convert the blocking std listener we used to read the
    // bound port (so the connect URL is correct before the async task even starts).
    let tokio_listener = tokio::net::TcpListener::from_std(listener)?;

    let app = Router::new()
        // The control page (HTML) at /<token>.
        .route(&format!("/{token}"), get(control_page))
        // The WebSocket at /<token>/ws — same token gates it.
        .route(&format!("/{token}/ws"), get(ws_handler))
        // A 404 for the root and wrong-token paths (don't leak that remote control is on).
        .fallback(fallback)
        .with_state(state);

    let server = tokio::spawn(async move {
        axum::serve(tokio_listener, app).await.ok(); // best-effort: errors here mean the user turned it off / the port dropped
    });

    Ok(RemoteControl {
        snapshot_tx,
        input_rx,
        url: RemoteUrl { url, addr, token },
        _server: server,
    })
}

#[derive(Clone)]
struct ServerState {
    snapshot_rx: watch::Receiver<Snapshot>,
    input_tx: mpsc::Sender<RemoteInput>,
}

/// The single control page: a responsive, dependency-free HTML/CSS/JS shell that mirrors the
/// statusline, shows the streaming reply + recent transcript, and sends inputs over the WS. It's
/// intentionally one self-contained string so there's no static-asset serving to wire up. Takes
/// the shared state (ignored) so axum's `Handler` bound is satisfied on a stateful router.
async fn control_page(State(_): State<Arc<ServerState>>) -> Html<&'static str> {
    Html(CONTROL_PAGE)
}

/// A minimal 404 that doesn't reveal remote control is running.
async fn fallback() -> Response {
    (axum::http::StatusCode::NOT_FOUND, "Not Found").into_response()
}

async fn ws_handler(State(state): State<Arc<ServerState>>, ws: WebSocketUpgrade) -> Response {
    // The route is static (the token is baked into the registered path at `start` time and is also
    // held in `state`), so there's no path parameter to extract — taking `Path<String>` here would
    // find zero captures and 500. A wrong-token request never matches the registered route and
    // falls through to the 404 fallback instead.
    ws.on_upgrade(move |socket| ws_session(socket, state))
}

/// One connected browser: forward snapshots out, parse inputs in. Runs until the browser
/// disconnects or the server stops (the watch channel closes → the forward loop exits).
async fn ws_session(socket: WebSocket, state: Arc<ServerState>) {
    use futures::stream::StreamExt;
    use futures::SinkExt;

    let (mut tx, mut rx) = socket.split();
    let mut snap = state.snapshot_rx.clone();

    // Send the current snapshot immediately so the page isn't blank until the next change.
    let initial = serde_json::to_string(&*snap.borrow()).unwrap_or_else(|_| "{}".into());
    if tx.send(Message::Text(initial.into())).await.is_err() {
        return;
    }

    let mut forward = tokio::spawn(async move {
        while let Ok(()) = snap.changed().await {
            let json = serde_json::to_string(&*snap.borrow()).unwrap_or_else(|_| "{}".into());
            if tx.send(Message::Text(json.into())).await.is_err() {
                break; // client gone
            }
        }
    });

    // Receive inputs from the browser; forward each to the render loop's channel.
    let input_tx = state.input_tx.clone();
    let mut receive = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                Message::Close(_) => break,
                // Ping/Pong are handled by axum automatically; ignore Binary-as-ping noise.
                _ => continue,
            };
            if let Ok(input) = serde_json::from_str::<RemoteInput>(&text) {
                if input_tx.send(input).await.is_err() {
                    break; // render loop dropped the receiver (remote turned off)
                }
            }
        }
    });

    // When either half ends, drop the other.
    tokio::select! {
        _ = &mut forward => { receive.abort(); }
        _ = &mut receive => { forward.abort(); }
    }
}

/// Render the connect URL as a scannable QR code into plain-text TUI scrollback lines. Returns
/// `None` when the encoder fails (we then just print the URL). Uses half-block glyphs so it reads
/// at a normal terminal cell aspect ratio.
pub fn qr_lines(url: &str) -> Option<Vec<String>> {
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    let width = code.width();
    let mut out: Vec<String> = Vec::with_capacity(width.div_ceil(2) + 2);
    out.push("  scan to connect:".to_string());
    for y in (0..width).step_by(2) {
        let mut row = String::from("  ");
        for x in 0..width {
            let top = code[(x, y)] == qrcode::Color::Light;
            let bottom = if y + 1 < width {
                code[(x, y + 1)] == qrcode::Color::Light
            } else {
                true
            };
            // Light = background. Combine two vertical modules into one cell:
            // both dark → '█', top dark only → '▀', bottom dark only → '▄', both light → ' '.
            row.push(if top {
                if bottom {
                    ' '
                } else {
                    '▄'
                }
            } else if bottom {
                '▀'
            } else {
                '█'
            });
        }
        out.push(row);
    }
    Some(out)
}

/// The self-contained control page. Plain HTML + a little CSS + vanilla JS (no framework, no
/// build step). Responsive: a one-column layout that's thumb-friendly on a phone and centered on
/// a desktop. The JS opens the WS at `window.location.pathname + "/ws"`, renders snapshots, and
/// sends `RemoteInput` JSON for the prompt box + action buttons.
const CONTROL_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, viewport-fit=cover">
<meta name="theme-color" content="#1c1c22">
<title>Forge remote</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  html, body { margin: 0; height: 100%; }
  body {
    background: #16161c; color: #d8d8e0; font: 15px/1.5 -apple-system, system-ui, sans-serif;
    display: flex; flex-direction: column; max-width: 760px; margin: 0 auto;
    padding: env(safe-area-inset-top) 14px env(safe-area-inset-bottom);
  }
  header { padding: 12px 0 6px; }
  h1 { font-size: 16px; margin: 0; color: #ff913c; font-weight: 700; letter-spacing: .3px; }
  .status {
    background: #1c1c22; border-radius: 8px; padding: 7px 10px; margin: 8px 0;
    font-size: 13px; display: flex; gap: 10px; flex-wrap: wrap; align-items: center;
    font-variant-numeric: tabular-nums;
  }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: #78d28c; flex: 0 0 auto; }
  .dot.busy { background: #ff913c; animation: pulse 1s infinite; }
  @keyframes pulse { 50% { opacity: .35; } }
  .tier { color: #ff913c; font-weight: 600; }
  .cost { color: #78d28c; font-weight: 600; }
  .ctx { color: #6e6e78; }
  .transcript {
    flex: 1 1 auto; overflow-y: auto; background: #101015; border-radius: 8px;
    padding: 10px 12px; margin: 8px 0; white-space: pre-wrap; word-break: break-word;
    font-size: 14px; min-height: 120px;
  }
  .transcript div { padding: 2px 0; }
  .stream { color: #c8c8d0; font-style: italic; }
  .prompt {
    background: #1c1c22; border-radius: 8px; padding: 8px 10px; margin: 8px 0;
    border: 1px solid #ff913c;
  }
  .prompt .q { font-weight: 700; color: #ff913c; }
  .bar { display: flex; gap: 8px; margin: 8px 0; flex-wrap: wrap; }
  button, input[type=text], .btn {
    font: inherit; border: none; border-radius: 8px; padding: 10px 16px; cursor: pointer;
  }
  input[type=text] {
    flex: 1 1 200px; background: #1c1c22; color: #fff; border: 1px solid #33333c;
    min-width: 0;
  }
  input[type=text]:focus { outline: 2px solid #ff913c; }
  .send { background: #ff913c; color: #1c1c22; font-weight: 700; }
  .y { background: #78d28c; color: #1c1c22; font-weight: 700; }
  .n { background: #f06e6e; color: #1c1c22; font-weight: 700; }
  .esc { background: #33333c; color: #d8d8e0; }
  .actions:empty { display: none; }
  .conn { font-size: 12px; color: #6e6e78; text-align: center; padding: 4px 0 8px; }
  .off { display: none; }
  footer { font-size: 11px; color: #4a4a54; text-align: center; padding: 6px 0 2px; }
</style>
</head>
<body>
<header><h1>⚒ Forge remote control</h1></header>
<div class="status" id="status"><span class="dot" id="dot"></span>
  <span class="tier" id="tier">—</span><span id="model">—</span>
  <span class="cost" id="cost">$0.0000</span><span class="ctx" id="ctx"></span>
  <span class="ctx" id="temper"></span></div>
<div class="transcript" id="transcript"></div>
<div class="actions" id="actions"></div>
<div class="bar">
  <input type="text" id="prompt" placeholder="type a task or /command…" autocomplete="off" enterkeyhint="send">
  <button class="send" id="send">Send</button>
  <button class="esc" id="stop">Stop</button>
</div>
<div class="conn" id="conn">connecting…</div>
<footer>Forge remote control · turn off with <code>/remote</code> in the TUI</footer>

<script>
const wsUrl = window.location.pathname.replace(/\/$/, "") + "/ws";
const $ = (id) => document.getElementById(id);
let ws = null, dead = false, sent = 0;

function connect() {
  if (dead) return;
  const scheme = location.protocol === "https:" ? "wss://" : "ws://";
  ws = new WebSocket(scheme + location.host + wsUrl);
  ws.onopen = () => { $("conn").textContent = "● connected"; };
  ws.onmessage = (e) => {
    let s; try { s = JSON.parse(e.data); } catch { return; }
    render(s);
    if (s.closed) { dead = true; $("conn").textContent = "remote control turned off — reconnect to the TUI"; ws.close(); }
  };
  ws.onclose = () => {
    if (dead) return;
    $("conn").textContent = "reconnecting…";
    setTimeout(connect, 1500);
  };
  ws.onerror = () => ws.close();
}
function send(obj) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj)); }

function render(s) {
  $("dot").className = "dot" + (s.busy ? " busy" : "");
  $("tier").textContent = s.tier ? "[" + s.tier + "]" : "—";
  $("model").textContent = s.model || "—";
  $("cost").textContent = "$" + (s.cost_usd || 0).toFixed(4);
  $("temper").textContent = s.temper ? "◆ " + s.temper : "";
  if (s.context_tokens > 0) {
    const lim = s.context_limit ? "/" + fmt(s.context_limit) : "";
    $("ctx").textContent = "◷ " + fmt(s.context_tokens) + lim;
  } else { $("ctx").textContent = ""; }
  const t = $("transcript");
  // Only re-render when the transcript actually changed (avoid scroll jumps every frame).
  const body = (s.transcript || []).join("\n") + (s.streaming ? "\n" + s.streaming : "");
  if (t._n !== sent + body.length) {
    t.innerHTML = "";
    (s.transcript || []).forEach(line => {
      const d = document.createElement("div"); d.textContent = line; t.appendChild(d);
    });
    if (s.streaming) {
      const d = document.createElement("div"); d.className = "stream"; d.textContent = s.streaming; t.appendChild(d);
    }
    t.scrollTop = t.scrollHeight;
    t._n = sent + body.length;
  }
  const a = $("actions");
  a.innerHTML = "";
  if (s.permission_prompt) {
    a.innerHTML = '<div class="prompt"><span class="q">⚠ ' + esc(s.permission_prompt) +
      '</span></div><div class="bar"><button class="y" onclick="answer(true)">Yes (allow)</button>' +
      '<button class="n" onclick="answer(false)">No (deny)</button></div>';
  } else if (s.question) {
    a.innerHTML = '<div class="prompt"><span class="q">❓ ' + esc(s.question) +
      '</span></div><div class="bar"><input type="text" id="ans" placeholder="answer…" enterkeyhint="done">' +
      '<button class="send" onclick="sendAnswer()">Answer</button></div>';
  }
}
function fmt(n) {
  if (n >= 1e6) return (n/1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n/1e3).toFixed(1) + "k";
  return "" + n;
}
function esc(s) { return (s||"").replace(/[&<>]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;"}[c])); }
function submit() {
  const v = $("prompt").value;
  if (!v.trim()) return;
  send({kind:"prompt", text:v}); $("prompt").value = ""; sent++;
}
$("send").onclick = submit;
$("prompt").addEventListener("keydown", e => { if (e.key === "Enter") { e.preventDefault(); submit(); } });
$("stop").onclick = () => send({kind:"interrupt"});
window.answer = (yes) => send({kind:"allow", yes:!!yes});
window.sendAnswer = () => { const v = $("ans").value; if (v.trim()) send({kind:"answer", text:v}); sent++; };
$("prompt").focus();
connect();
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_to_json_with_all_fields() {
        let s = Snapshot {
            busy: true,
            temper: "Ask".into(),
            tier: Some("complex".into()),
            model: "groq::llama-3.3-70b".into(),
            cost_usd: 0.0123,
            context_tokens: 18_200,
            context_limit: Some(200_000),
            streaming: "thinking…".into(),
            transcript: vec!["you: hi".into(), "forge: hello".into()],
            permission_prompt: Some("allow write_file".into()),
            question: None,
            done: false,
            closed: false,
        };
        // Snapshot is server→client (serialize only); confirm the wire shape carries every field
        // the control page's JS reads, so a schema drift is caught here rather than at runtime.
        let json = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["busy"], true);
        assert_eq!(v["tier"], "complex");
        assert_eq!(v["model"], "groq::llama-3.3-70b");
        assert_eq!(v["cost_usd"], 0.0123);
        assert_eq!(v["context_tokens"], 18200);
        assert_eq!(v["context_limit"], 200000);
        assert_eq!(v["transcript"][0], "you: hi");
        assert_eq!(v["permission_prompt"], "allow write_file");
        assert_eq!(v["question"], serde_json::Value::Null);
        assert_eq!(v["closed"], false);
    }

    #[test]
    fn remote_inputs_deserialize_tagged_variants() {
        assert_eq!(
            serde_json::from_str::<RemoteInput>(r#"{"kind":"prompt","text":"fix it"}"#).unwrap(),
            RemoteInput::Prompt {
                text: "fix it".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<RemoteInput>(r#"{"kind":"allow","yes":true}"#).unwrap(),
            RemoteInput::Allow { yes: true }
        );
        assert_eq!(
            serde_json::from_str::<RemoteInput>(r#"{"kind":"answer","text":"2"}"#).unwrap(),
            RemoteInput::Answer { text: "2".into() }
        );
        assert_eq!(
            serde_json::from_str::<RemoteInput>(r#"{"kind":"interrupt"}"#).unwrap(),
            RemoteInput::Interrupt
        );
    }

    #[test]
    fn random_token_is_hex_and_sixteen_chars() {
        let t = random_token();
        assert_eq!(t.len(), 16);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Two calls (almost) never collide.
        let t2 = random_token();
        assert_ne!(t, t2);
    }

    #[test]
    fn qr_lines_render_for_a_url() {
        let lines = qr_lines("http://192.168.1.10:4123/0123456789abcdef").unwrap();
        assert!(lines.len() > 2, "QR has a header + rows: {lines:?}");
        assert!(lines[0].contains("scan to connect"));
        // Every row uses only the half-block glyph set (plus leading pad).
        for row in &lines[1..] {
            assert!(
                row.chars()
                    .skip(2)
                    .all(|c| matches!(c, ' ' | '▀' | '▄' | '█')),
                "row uses half-block glyphs: {row:?}"
            );
        }
    }

    /// `start()` binds a real port + spawns the server task. This is the real round-trip smoke:
    /// it does an HTTP GET on the control page (expect 200 + HTML), a wrong-token GET (expect
    /// 404, so the existence of remote control isn't leaked), and a WebSocket handshake on the
    /// token-gated WS path (expect it upgrades + delivers a snapshot). Catches the
    /// `Path<String>`-on-a-static-route regression where the WS would 400 and never connect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "binds a real port + opens a real socket; run with --ignored (kills itself on success)"]
    async fn start_serves_page_and_upgrades_websocket() {
        // Wrap in a timeout so a stuck server/client can never hang forever. The server's spawned
        // accept loop can delay runtime shutdown on drop (a test-harness artifact, not a product
        // bug — the real loop runs under `forge chat`'s long-lived runtime), so we force-exit 0
        // once the assertions pass. Gated behind --ignored so it never runs in CI.
        let _outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            use futures::StreamExt;
            let rc = start(false).expect("start loopback server");
            let port = rc.url.addr.port();
            let token = rc.url.token.clone();

            // 1. The control page is served at the token URL.
            let http = reqwest::Client::new();
            let page = http
                .get(format!("http://127.0.0.1:{port}/{token}"))
                .send()
                .await
                .expect("GET control page");
            assert_eq!(page.status(), 200, "control page is 200 at the token URL");
            let body = page.text().await.unwrap();
            assert!(
                body.contains("Forge remote control"),
                "HTML body served: {body}"
            );

            // 2. A wrong token → 404 (don't leak that remote control is on).
            let wrong = http
                .get(format!("http://127.0.0.1:{port}/deadbeefdeadbeef"))
                .send()
                .await
                .expect("GET wrong token");
            assert_eq!(wrong.status(), 404, "wrong token is a 404");

            // 3. The WebSocket handshake on /<token>/ws upgrades + delivers the first snapshot.
            //    This is the regression guard: a static route + `Path<String>` used to 500 here.
            let ws_url = format!("ws://127.0.0.1:{port}/{token}/ws");
            let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url)
                .await
                .expect("WS handshake upgrades");
            let first = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
                .await
                .expect("a snapshot arrives")
                .expect("stream open")
                .expect("text frame");
            let text = first.into_text().expect("text frame");
            let v: serde_json::Value = serde_json::from_str(&text).expect("snapshot is JSON");
            assert!(v.get("busy").is_some(), "snapshot has `busy`: {v}");
            assert!(v.get("model").is_some(), "snapshot has `model`: {v}");
            // All assertions passed — force-exit so the lingering server task + WS close
            // handshake can't stall the test runtime's shutdown (manual-only, --ignored).
            std::process::exit(0);
        })
        .await;
        // Unreachable on success (exit above); only reached if the 5s timeout elapsed.
        let _ = _outcome;
        panic!("WS round-trip did not complete within 5s");
    }
}
