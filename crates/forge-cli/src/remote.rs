//! Remote control — drive a running `forge chat` session from a phone or desktop browser.
//!
//! `/remote` (alias `/rc`) starts a tiny HTTP + WebSocket server bound to `0.0.0.0:<ephemeral>`
//! (LAN-reachable). A single HTML control page is served at a token-gated URL (printed into the
//! TUI scrollback + rendered as a QR code so a phone can scan-to-connect). One bidirectional
//! WebSocket carries the live [`Snapshot`] (model · busy · cost · context · statusline · the
//! recent transcript edge) to the browser and [`RemoteInput`] (prompt / answer / interrupt) back.
//!
//! `--local` binds loopback only (control from this machine); `--anywhere` binds loopback and
//! pipes it through a public tunnel (cloudflared / ngrok / bore, whichever is installed) so the
//! page is reachable from any network with NO manual router port-forwarding — the connect URL is
//! then a public `https://…/<token>`. See [`Exposure`] + [`start_anywhere`].
//!
//! The design goals are: *easy* (one slash command, no install, works from any browser), and
//! *accessible on mobile + desktop* (a responsive, low-friction control page that needs no app).
//! The server is in-process so it reuses the running Session + presenter channel — no second
//! process, no IPC, no keys to configure. Security is a random token in the URL path: a LAN peer
//! (or, under `--anywhere`, anyone on the internet) without the token can't drive the session —
//! so the token is genuinely load-bearing once a public tunnel is open.
//!
//! ## TLS (LAN bind only)
//!
//! When binding to `0.0.0.0` (LAN), the server generates a self-signed certificate at startup and
//! serves HTTPS so the access token doesn't travel in cleartext. The cert's SHA-256 fingerprint is
//! printed alongside the connect URL so the user can verify it in the browser's cert dialog.
//! Loopback (`--local`) stays plain HTTP — the connection never leaves the machine. Tunnel modes
//! are unchanged — the provider (cloudflared / ngrok) already terminates TLS. If TLS setup fails
//! for any reason the server falls back to plain HTTP with a loud warning rather than refusing to
//! start.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// TLS helpers (LAN bind)
// ---------------------------------------------------------------------------

/// A self-signed certificate + private key generated at startup for the LAN HTTPS server.
struct SelfSignedCert {
    /// PEM-encoded certificate (fed to RustlsConfig).
    cert_pem: Vec<u8>,
    /// PEM-encoded private key (fed to RustlsConfig).
    key_pem: Vec<u8>,
    /// SHA-256 fingerprint of the DER-encoded certificate, colon-separated uppercase hex.
    /// e.g. `"AB:CD:EF:…"` — shown to the user so they can verify the cert in their browser.
    fingerprint: String,
}

/// Generate a self-signed TLS certificate valid for the given SANs (Subject Alternative Names).
/// Returns `Err` only if rcgen itself fails, which shouldn't happen with valid input.
fn generate_self_signed(sans: Vec<String>) -> Result<SelfSignedCert, rcgen::Error> {
    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(sans)?;

    // DER bytes → SHA-256 fingerprint
    let der: &[u8] = cert.der();
    let fingerprint = sha256_fingerprint(der);

    Ok(SelfSignedCert {
        cert_pem: cert.pem().into_bytes(),
        key_pem: signing_key.serialize_pem().into_bytes(),
        fingerprint,
    })
}

/// Compute a SHA-256 digest over `bytes` and return it as uppercase colon-separated hex,
/// e.g. `"AB:CD:EF:…"`. Pure-Rust, no external crypto dep — we just need a fingerprint for
/// display, not a security-critical MAC, so a straightforward byte-by-byte implementation is fine.
fn sha256_fingerprint(bytes: &[u8]) -> String {
    // SHA-256 is available via rustls/ring which are already in the dep tree, but rather than
    // adding another direct dep (ring or sha2) we implement the digest inline using the
    // `rustls` re-export of the ring digest via `rustls::crypto::ring`. However, the cleanest
    // zero-new-dep approach is to use `std` — which has no SHA-256. Instead we rely on `rcgen`
    // pulling in `ring` (which is already compiled) and call it through the public `rcgen` API.
    //
    // Simplest alternative that truly adds no dep: use rustls-provided digest. rustls 0.23
    // exposes `rustls::crypto::CryptoProvider` but not a raw hash. The actual zero-dep path is
    // to implement SHA-256 ourselves — but that's many lines and error-prone. We instead just
    // depend on the `ring` crate (already an indirect dep of rustls + rcgen) via the `rcgen`
    // feature or we access it through `axum-server`'s already-compiled `rustls` stack.
    //
    // Practical decision: use `ring::digest` which is guaranteed to be compiled (it's a dep of
    // rustls 0.23 via the default ring provider). We access it via the re-exported path from
    // `rcgen`'s transitive dep — but that requires adding `ring` to Cargo.toml.
    //
    // To keep this truly dep-free we implement a minimal SHA-256 inline. The implementation
    // follows FIPS 180-4 and is only ~80 lines — acceptable for a display fingerprint.
    let digest = sha256_raw(bytes);
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Minimal SHA-256 implementation (FIPS 180-4). Used only for cert fingerprint display.
/// Not constant-time; not intended for HMAC or key derivation.
fn sha256_raw(data: &[u8]) -> [u8; 32] {
    // Round constants (first 32 bits of the fractional parts of the cube roots of the first 64 primes)
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    // Initial hash values (first 32 bits of the fractional parts of the square roots of the first 8 primes)
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: pad the message
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) chunk
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            *word = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// How the local server is exposed to a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Exposure {
    /// Bind `0.0.0.0` — reachable from the LAN (the original `/remote` default).
    #[default]
    Lan,
    /// Bind `127.0.0.1` only — control from this machine.
    Local,
    /// Bind loopback and pipe it through a public tunnel so any browser, anywhere, can reach it.
    /// No manual router port-forwarding: the tunnel (cloudflared/ngrok/bore) punches through NAT.
    Anywhere,
}

impl From<forge_config::RemoteAuto> for Exposure {
    /// Map the `[remote] auto` config value to a server exposure. `Off` has no exposure (the
    /// caller only converts after [`forge_config::RemoteConfig::startup_exposure`] returns
    /// `Some`), so it falls back to the safest bind (loopback).
    fn from(a: forge_config::RemoteAuto) -> Self {
        match a {
            forge_config::RemoteAuto::Lan => Exposure::Lan,
            forge_config::RemoteAuto::Anywhere => Exposure::Anywhere,
            forge_config::RemoteAuto::Local | forge_config::RemoteAuto::Off => Exposure::Local,
        }
    }
}

/// A public-tunnel provider Forge can drive if it's installed. Probed in priority order: the
/// first one found on `PATH` is used. Each is free to run for a session; cloudflared/ngrok give
/// HTTPS (the page's JS auto-picks `wss://`), bore gives plain TCP (`ws://`). All three proxy the
/// HTTP WebSocket upgrade transparently, so the existing control page + token gate work unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TunnelKind {
    /// `cloudflared tunnel --url http://localhost:PORT` → `https://<rand>.trycloudflare.com`.
    /// Free, no account, HTTPS, supports WebSocket. Preferred.
    Cloudflared,
    /// `ngrok http PORT` → `https://<id>.ngrok-free.app` (needs a one-time `ngrok config add-authtoken`).
    Ngrok,
    /// `bore local PORT --to bore.pub` → `bore.pub:<port>` (plain TCP, no TLS, no account).
    Bore,
}

impl TunnelKind {
    /// All providers in probe priority order.
    const ALL: [Self; 3] = [Self::Cloudflared, Self::Ngrok, Self::Bore];

    /// The binary name to look for on `PATH`.
    fn binary(self) -> &'static str {
        match self {
            Self::Cloudflared => "cloudflared",
            Self::Ngrok => "ngrok",
            Self::Bore => "bore",
        }
    }

    /// A one-line human label for scrollback notes.
    fn label(self) -> &'static str {
        match self {
            Self::Cloudflared => "cloudflared (trycloudflare.com)",
            Self::Ngrok => "ngrok",
            Self::Bore => "bore.pub",
        }
    }

    /// Build the argv that points the tunnel at `local_port`.
    fn argv(self, local_port: u16) -> Vec<String> {
        match self {
            Self::Cloudflared => vec![
                "tunnel".into(),
                "--url".into(),
                format!("http://localhost:{local_port}"),
            ],
            Self::Ngrok => vec!["http".into(), local_port.to_string()],
            // bore: `local <port> --to bore.pub` — the public instance. No account, no secret.
            Self::Bore => vec![
                "local".into(),
                local_port.to_string(),
                "--to".into(),
                "bore.pub".into(),
            ],
        }
    }

    /// Pull the public URL out of a line of the tunnel's stdout/stderr. Each provider prints it
    /// differently; these patterns are matched against the *verified* output formats:
    /// - cloudflared logs the `https://…trycloudflare.com` URL in a log line on stderr.
    /// - ngrok prints `Forwarding  https://<id>.ngrok-free.app -> http://localhost:PORT`.
    /// - bore logs `listening at bore.pub:<port>` (plain TCP → an http:// URL).
    fn parse_url(self, line: &str) -> Option<String> {
        match self {
            Self::Cloudflared => {
                // e.g. `... INF +-----------------------------------------+` then a line with the URL,
                // or `Your quick Tunnel has been created ... https://x.trycloudflare.com`. Match any
                // trycloudflare.com https URL on the line.
                line.split_whitespace()
                    .find(|tok| tok.starts_with("https://") && tok.contains("trycloudflare.com"))
                    .map(|t| {
                        t.trim_matches(|c: char| {
                            !c.is_ascii_alphanumeric()
                                && c != ':'
                                && c != '/'
                                && c != '.'
                                && c != '-'
                        })
                        .to_string()
                    })
            }
            Self::Ngrok => {
                // `Forwarding  https://abc.ngrok-free.app -> http://localhost:8080`
                line.split_whitespace()
                    .find(|tok| {
                        tok.starts_with("https://")
                            && (tok.contains("ngrok.io")
                                || tok.contains("ngrok-free.app")
                                || tok.contains("ngrok.app"))
                    })
                    .map(|t| t.trim_end_matches(',').to_string())
            }
            Self::Bore => {
                // `listening at bore.pub:40123` → http URL (plain TCP, no TLS).
                if let Some(idx) = line.find("bore.pub:") {
                    let rest = &line[idx..];
                    let port: String = rest
                        .chars()
                        .skip("bore.pub:".len())
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
                        return Some(format!("http://bore.pub:{port}"));
                    }
                }
                None
            }
        }
    }
}

/// Which tunnel provider (if any) is installed and on `PATH`. Probes each in priority order.
fn detect_tunnel() -> Option<TunnelKind> {
    TunnelKind::ALL
        .into_iter()
        .find(|k| which(k.binary()).is_some())
}

/// Best-effort `which`: is `bin` resolvable on `PATH`? Uses `std::env::var` + a manual search so
/// we don't pull a `which` crate; on Windows it also checks for `.exe`/`.cmd`/`.bat` suffixes.
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts = if cfg!(windows) {
        vec!["", ".exe", ".cmd", ".bat"]
    } else {
        vec![""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Spawn a tunnel of `kind` pointing at `local_port`. Returns the public URL (parsed from the
/// tunnel's output) + the child handle (so the caller can kill it when remote control turns off).
/// Fails if the child can't start or no URL appears within the timeout (the tunnel is then killed).
async fn spawn_tunnel(
    kind: TunnelKind,
    local_port: u16,
) -> std::io::Result<(String, tokio::process::Child)> {
    use tokio::io::AsyncReadExt;

    let bin = which(kind.binary()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} not on PATH", kind.binary()),
        )
    })?;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(kind.argv(local_port))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;

    // Merge stdout + stderr so the URL (whichever stream it lands on) is seen. cloudflared prints
    // the URL on stderr; ngrok on stdout; bore on stderr (via tracing). Read both concurrently.
    // The readers drain to EOF (the child's exit) regardless of whether anyone is still receiving:
    // once we have the URL we stop reading `rx`, but a chatty tunnel keeps logging — if we stopped
    // draining its pipe, a full pipe buffer would block the tunnel process and stall forwarding.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Generous buffer: the URL appears within the first handful of log lines, but the receiver
    // may not be polling yet — a deep buffer means an early burst can't drop the URL line.
    let (tx, mut rx) = mpsc::channel::<String>(256);

    let tx1 = tx.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    for line in chunk.lines() {
                        // Non-blocking: drop the line if the buffer is full or rx is gone, but NEVER
                        // block the reader — a blocked reader stops draining the pipe (deadlock).
                        let _ = tx1.try_send(line.to_string());
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    for line in chunk.lines() {
                        // Non-blocking (see stdout reader): keep draining stderr to EOF regardless.
                        let _ = tx.try_send(line.to_string());
                    }
                }
            }
        }
    });

    // Wait up to 20s for a recognizable public URL line. Tunnels take a few seconds to register;
    // 20s is generous without hanging forever on a broken/misconfigured install.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            let _ = child.kill().await;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} did not print a public URL within 20s", kind.binary()),
            ));
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(line)) => {
                if let Some(url) = kind.parse_url(&line) {
                    return Ok((url, child));
                }
            }
            Ok(None) => break, // both readers closed (child exited early)
            Err(_) => {}       // timeout on this recv; loop checks the deadline
        }
    }
    let status = child.try_wait().ok().flatten();
    let _ = child.kill().await;
    Err(std::io::Error::other(format!(
        "{} exited before printing a URL{}",
        kind.binary(),
        status.map(|s| format!(" (status {s})")).unwrap_or_default()
    )))
}

/// A token-gated URL is printed into the TUI so the user can scan/click to connect.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `token` is read by tests + serves as a stable handle field
pub struct RemoteUrl {
    /// `http(s)://host:port/TOKEN` — the full connect URL (host resolved best-effort).
    pub url: String,
    /// The LAN-visible host:port, for the scrollback note ("listening on …").
    pub addr: SocketAddr,
    /// The random path token (also the WS auth key).
    pub token: String,
    /// SHA-256 fingerprint of the TLS certificate (LAN HTTPS only), colon-separated uppercase
    /// hex. `None` for loopback (HTTP) and tunnel modes (provider terminates TLS).
    pub tls_fingerprint: Option<String>,
}

/// Wire-protocol version for the remote control page ⇄ server contract. Bumped whenever the
/// [`Snapshot`] / [`RemoteInput`] shape changes in a way the page must know about; the page
/// shows a "refresh to update" hint when its bundled version and the server's disagree.
pub const PROTOCOL_VERSION: u32 = 2;

/// Hard cap on a single inbound WebSocket frame (a [`RemoteInput`]). Inputs are short prompts or
/// answers; anything larger is dropped to bound memory + parse cost from a hostile/buggy client.
const MAX_INPUT_BYTES: usize = 256 * 1024;

/// One tracked task in the live task list, projected for the wire (status as a stable word).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapTask {
    pub title: String,
    /// "pending" | "in_progress" | "done".
    pub status: String,
}

/// One live subagent row, projected for the wire.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapSubagent {
    pub agent: String,
    pub task: String,
    pub model: Option<String>,
    /// Trailing edge of the child's streamed activity.
    pub last: String,
    pub done: bool,
    pub cost: f64,
}

/// One selectable option of a pending AskUserQuestion, so the page can render tappable buttons
/// instead of forcing the user to type a number on a phone keyboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapOption {
    pub label: String,
    pub description: String,
}

/// One frame of visible state broadcast to every connected browser, so the control page mirrors
/// the TUI statusline + the tail of the conversation. Cheap to build (plain strings) and JSON, so
/// a phone renders it without any client-side framework.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    /// Wire-protocol version (see [`PROTOCOL_VERSION`]); the page warns on a mismatch.
    pub protocol: u32,
    /// The active session id — shown in the header so the operator knows which session they drive.
    pub session_id: String,
    /// The working directory the session runs in (header context).
    pub cwd: String,
    /// How the server is exposed: "loopback" | "LAN" | "public (provider)".
    pub exposure: String,
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
    /// The live task list (`update_tasks`) — drives the remote task panel.
    pub tasks: Vec<SnapTask>,
    /// Live subagents in the current `spawn_agents` batch.
    pub subagents: Vec<SnapSubagent>,
    /// Prompts the operator queued while a turn was running (shown so nothing looks dropped).
    pub queued: Vec<String>,
    /// A pending permission prompt, if the turn is blocked on a y/n.
    pub permission_prompt: Option<String>,
    /// A pending AskUserQuestion, if the turn is blocked on a choice.
    pub question: Option<String>,
    /// The options for a pending AskUserQuestion (tappable buttons on the page).
    pub question_options: Vec<SnapOption>,
    /// Whether the pending question accepts a free-text answer in addition to its options.
    pub question_allow_other: bool,
    /// `true` once remote control has been turned off (tells the page to stop reconnecting).
    pub closed: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            session_id: String::new(),
            cwd: String::new(),
            exposure: String::new(),
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
            tasks: Vec::new(),
            subagents: Vec::new(),
            queued: Vec::new(),
            permission_prompt: None,
            question: None,
            question_options: Vec::new(),
            question_allow_other: false,
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
    /// The public-tunnel child process (`--anywhere` only). `kill_on_drop`, so dropping the
    /// handle tears the tunnel down with the server. `None` for LAN/loopback exposure.
    _tunnel: Option<tokio::process::Child>,
    /// The tunnel provider's human label (`--anywhere` only), for the scrollback note.
    pub tunnel: Option<&'static str>,
    /// Set from the spawned server task if a `Lan` bind's TLS setup fails *after* [`start`] already
    /// returned an `https://` URL (see [`Self::tls_degraded`]) — the connect URL and cert
    /// fingerprint were fixed at return time and can't be corrected in place, so this is checked
    /// separately wherever the exposure is reported (e.g. the remote-page header).
    tls_degraded: Arc<AtomicBool>,
}

impl RemoteControl {
    /// True once a `Lan`-exposure bind's TLS config build has failed and the server fell back to
    /// plain HTTP — meaning the token is travelling in cleartext despite the `https://` connect
    /// URL handed out at start time. Always `false` for `Local`/`Anywhere` (no TLS is attempted).
    pub fn tls_degraded(&self) -> bool {
        self.tls_degraded.load(Ordering::Relaxed)
    }
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
    // 16 hex chars (64 bits) sourced from the OS CSPRNG via `rand::random` (already a workspace
    // dependency — see `forge-config::oauth`). This is genuinely load-bearing under `--anywhere`,
    // where the token is the sole authentication for a public, internet-reachable control
    // channel, so it must not be derived from guessable/low-entropy inputs like the process
    // start time or pid.
    format!("{:016x}", rand::random::<u64>())
}

/// Best-effort LAN hostname for the connect URL. We keep it dependency-free and just return the
/// numeric IP — it always resolves from the phone, and avoids a DNS-lookup edge case where the
/// machine's hostname isn't resolvable on the LAN (which would yield a dead QR code).
fn lan_host(addr: SocketAddr) -> String {
    addr.ip().to_string()
}

/// Start the remote-control server. The returned [`RemoteControl`] is moved into the render loop;
/// dropping it stops the server and frees the port. [`Exposure`] selects the bind address:
/// `Lan` → `0.0.0.0` (LAN-reachable, HTTPS with self-signed cert), `Local`/`Anywhere` →
/// `127.0.0.1` (loopback, plain HTTP). `Anywhere` binds loopback because the public tunnel
/// ([`start_anywhere`]) provides the public exposure; this fn does NOT spawn the tunnel (it's
/// sync) — use [`start_anywhere`] for that.
///
/// **TLS**: For the `Lan` exposure the server generates a self-signed certificate and serves
/// HTTPS so the access token never travels in cleartext over the LAN. The cert fingerprint is
/// included in the returned [`RemoteUrl`] so it can be shown to the user. If TLS setup fails
/// (cert generation error or RustlsConfig build error) the server falls back to plain HTTP with a
/// `tracing::warn!` rather than failing to start.
pub fn start(exposure: Exposure) -> std::io::Result<RemoteControl> {
    let token = random_token();
    let bind_ip: std::net::IpAddr = match exposure {
        Exposure::Lan => std::net::Ipv4Addr::UNSPECIFIED.into(),
        Exposure::Local | Exposure::Anywhere => std::net::Ipv4Addr::LOCALHOST.into(),
    };
    // Port 0 → the OS picks a free ephemeral port (no clashes, no config).
    let listener = std::net::TcpListener::bind((bind_ip, 0))?;
    let addr = listener.local_addr()?;
    let host = lan_host(addr);

    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::default());
    let (input_tx, input_rx) = mpsc::channel::<RemoteInput>(64);

    let base = format!("/{token}");
    let state = Arc::new(ServerState {
        snapshot_rx: snapshot_rx.clone(),
        input_tx,
        base: base.clone(),
    });

    let app = Router::new()
        // The control page (HTML) at /<token> and /<token>/ — the slashed form is the PWA
        // `start_url` so the installed app launches inside the service-worker scope.
        .route(&base, get(control_page))
        .route(&format!("{base}/"), get(control_page))
        // The WebSocket at /<token>/ws — same token gates it.
        .route(&format!("{base}/ws"), get(ws_handler))
        // PWA assets (token-scoped) so the page installs to a phone home screen + runs standalone.
        .route(&format!("{base}/manifest.webmanifest"), get(manifest))
        .route(&format!("{base}/sw.js"), get(service_worker))
        .route(&format!("{base}/icon.svg"), get(icon))
        // A 404 for the root and wrong-token paths (don't leak that remote control is on).
        .fallback(fallback)
        .with_state(state);

    // For the LAN exposure, attempt TLS with a self-signed certificate so the access token
    // doesn't travel in cleartext. Fall back to plain HTTP on any error.
    if exposure == Exposure::Lan {
        // SANs: the numeric LAN IP + localhost (so a browser connecting by hostname also works).
        let sans = vec![host.clone(), "localhost".to_string()];
        match generate_self_signed(sans) {
            Ok(tls) => {
                let fingerprint = tls.fingerprint.clone();
                let cert_pem = tls.cert_pem;
                let key_pem = tls.key_pem;
                let url = format!("https://{host}:{}/{}", addr.port(), token);
                // axum-server calls tokio::net::TcpListener::from_std internally, which
                // requires the listener to already be in nonblocking mode.
                listener.set_nonblocking(true)?;

                // axum-server::from_tcp_rustls takes a std::net::TcpListener (non-async).
                // We build the RustlsConfig inside the spawned async task because
                // RustlsConfig::from_pem is async (it spawns blocking work internally).
                // `start()` already committed to an `https://` URL above (before this task even
                // runs), so a fallback here can't change the connect URL — `tls_degraded` is how
                // the render loop finds out the token is actually travelling in cleartext.
                let tls_degraded = Arc::new(AtomicBool::new(false));
                let tls_degraded_task = tls_degraded.clone();
                let server = tokio::spawn(async move {
                    match axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem).await {
                        Ok(tls_config) => {
                            match axum_server::from_tcp_rustls(listener, tls_config) {
                                Ok(server) => {
                                    server.serve(app.into_make_service()).await.ok();
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "remote: TLS server setup failed ({e}), \
                                         LAN remote control is unavailable"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "remote: TLS config build failed ({e}); \
                                 falling back to plain HTTP — token will travel in cleartext"
                            );
                            tls_degraded_task.store(true, Ordering::Relaxed);
                            // Fall back: rebuild listener (the original was moved). We can't
                            // easily un-move it, so we open a new one on the same addr.
                            // In the unlikely event that fails, just log and exit the task.
                            match std::net::TcpListener::bind(addr) {
                                Ok(fb_listener) => {
                                    if let Ok(tl) = tokio::net::TcpListener::from_std(fb_listener) {
                                        axum::serve(tl, app).await.ok();
                                    }
                                }
                                Err(bind_err) => {
                                    tracing::warn!(
                                        "remote: fallback HTTP bind also failed ({bind_err}); \
                                         remote control unavailable"
                                    );
                                }
                            }
                        }
                    }
                });

                return Ok(RemoteControl {
                    snapshot_tx,
                    input_rx,
                    url: RemoteUrl {
                        url,
                        addr,
                        token,
                        tls_fingerprint: Some(fingerprint),
                    },
                    _server: server,
                    _tunnel: None,
                    tls_degraded,
                    tunnel: None,
                });
            }
            Err(e) => {
                tracing::warn!(
                    "remote: self-signed cert generation failed ({e}); \
                     falling back to plain HTTP on the LAN — token will be sent in cleartext"
                );
                // Fall through to the plain HTTP path below.
            }
        }
    }

    // Plain HTTP path: loopback (--local / --anywhere) and LAN fallback.
    // axum wants a tokio TcpListener; convert the blocking std listener we used to read the
    // bound port (so the connect URL is correct before the async task even starts).
    listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::TcpListener::from_std(listener)?;
    let url = format!("http://{host}:{}/{}", addr.port(), token);

    let server = tokio::spawn(async move {
        axum::serve(tokio_listener, app).await.ok(); // best-effort: errors here mean the user turned it off / the port dropped
    });

    Ok(RemoteControl {
        snapshot_tx,
        input_rx,
        url: RemoteUrl {
            url,
            addr,
            token,
            tls_fingerprint: None,
        },
        _server: server,
        _tunnel: None,
        tunnel: None,
        tls_degraded: Arc::new(AtomicBool::new(false)),
    })
}

/// Start the server on loopback and pipe it through a public tunnel so any browser, anywhere, can
/// reach it — no manual router port-forwarding. Probes for an installed tunnel CLI
/// (cloudflared → ngrok → bore) and points it at the bound port; the returned [`RemoteControl`]'s
/// `url` is the PUBLIC `https://…/<token>` (or `http://bore.pub:port/<token>`), and it owns the
/// tunnel child (killed on drop). Errors if no tunnel tool is installed or the tunnel never
/// publishes a URL — the caller surfaces an install hint.
pub async fn start_anywhere() -> std::io::Result<RemoteControl> {
    let kind = detect_tunnel().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no tunnel tool found on PATH — install one of: cloudflared, ngrok, or bore",
        )
    })?;
    let mut rc = start(Exposure::Anywhere)?;
    let port = rc.url.addr.port();
    let (public, child) = spawn_tunnel(kind, port).await?;
    // The control page lives at `/<token>`; the tunnel forwards the whole path, so the public
    // connect URL is the tunnel base + the same token gate.
    rc.url.url = format!("{}/{}", public.trim_end_matches('/'), rc.url.token);
    rc._tunnel = Some(child);
    rc.tunnel = Some(kind.label());
    Ok(rc)
}

#[derive(Clone)]
struct ServerState {
    snapshot_rx: watch::Receiver<Snapshot>,
    input_tx: mpsc::Sender<RemoteInput>,
    /// The token-gated base path (`/<token>`) — injected into the page + manifest so every URL
    /// (WS, PWA assets, start_url) is correct under a tunnel/LAN host without the page guessing.
    base: String,
}

/// The single control page: a responsive, dependency-free HTML/CSS/JS shell that mirrors the
/// statusline, shows the streaming reply + recent transcript, and sends inputs over the WS. It's
/// intentionally one self-contained string so there's no static-asset serving to wire up. Takes
/// the shared state (ignored) so axum's `Handler` bound is satisfied on a stateful router.
async fn control_page(State(state): State<Arc<ServerState>>) -> Html<String> {
    Html(CONTROL_PAGE.replace("__BASE__", &state.base))
}

/// The token-scoped PWA manifest (`start_url`/`scope` baked to this session's path).
async fn manifest(State(state): State<Arc<ServerState>>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/manifest+json",
        )],
        manifest_json(&state.base),
    )
        .into_response()
}

/// The service worker that makes the page installable (its scope is this session's token path).
async fn service_worker() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        SERVICE_WORKER,
    )
        .into_response()
}

/// The app icon (inline SVG; no binary asset to serve).
async fn icon() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        ICON_SVG,
    )
        .into_response()
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
            // Drop oversized frames: a remote input is a short prompt/answer, never a megabyte.
            // Caps memory + parse work from a hostile or buggy client on a public tunnel.
            if text.len() > MAX_INPUT_BYTES {
                continue;
            }
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
<meta name="theme-color" content="#16161c">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="apple-mobile-web-app-title" content="Forge">
<link rel="manifest" href="__BASE__/manifest.webmanifest">
<link rel="apple-touch-icon" href="__BASE__/icon.svg">
<link rel="icon" href="__BASE__/icon.svg">
<title>Forge remote</title>
<style>
  :root { color-scheme: dark; --bg:#16161c; --panel:#1c1c22; --ink:#d8d8e0; --dim:#6e6e78; --acc:#ff913c; --ok:#78d28c; --no:#f06e6e; }
  * { box-sizing: border-box; -webkit-tap-highlight-color: transparent; }
  html, body { margin: 0; height: 100%; }
  body {
    background: var(--bg); color: var(--ink); font: 15px/1.5 -apple-system, system-ui, "Segoe UI", sans-serif;
    display: flex; flex-direction: column; max-width: 820px; margin: 0 auto; height: 100dvh;
    padding: env(safe-area-inset-top) 12px calc(env(safe-area-inset-bottom) + 4px);
  }
  #banner { display:none; background:#3a2a12; color:#ffd9a8; border:1px solid var(--acc);
    border-radius:8px; padding:8px 10px; margin:8px 0 0; font-size:13px; }
  header { display:flex; align-items:center; gap:8px; padding: 10px 2px 4px; }
  h1 { font-size: 16px; margin: 0; color: var(--acc); font-weight: 700; letter-spacing:.3px; flex:1 1 auto; }
  .iconbtn { background:none; border:none; font-size:18px; cursor:pointer; padding:4px; line-height:1; }
  .meta { display:flex; gap:8px; flex-wrap:wrap; align-items:center; font-size:12px; color:var(--dim);
    padding:0 2px 2px; font-variant-numeric:tabular-nums; }
  .badge { background:var(--panel); border-radius:6px; padding:2px 7px; }
  .badge.pub { background:#3a1c1c; color:#ffb0b0; }
  .status {
    background: var(--panel); border-radius: 8px; padding: 7px 10px; margin: 6px 0;
    font-size: 13px; display: flex; gap: 10px; flex-wrap: wrap; align-items: center; font-variant-numeric: tabular-nums;
  }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--ok); flex: 0 0 auto; }
  .dot.busy { background: var(--acc); animation: pulse 1s infinite; }
  @keyframes pulse { 50% { opacity: .35; } }
  .tier { color: var(--acc); font-weight: 600; }
  .cost { color: var(--ok); font-weight: 600; }
  .ctx { color: var(--dim); }
  .tabs { display:flex; gap:6px; margin:6px 0 0; }
  .tab { flex:1 1 0; background:var(--panel); color:var(--dim); border:none; border-radius:8px 8px 0 0;
    padding:9px 6px; font:inherit; font-size:13px; font-weight:600; cursor:pointer; }
  .tab.on { color:var(--acc); background:#101015; box-shadow: inset 0 -2px 0 var(--acc); }
  .tab .n { color:var(--ink); font-weight:700; }
  .panel { flex:1 1 auto; overflow-y:auto; background:#101015; border-radius:0 0 8px 8px;
    padding:10px 12px; margin:0 0 6px; min-height:120px; -webkit-overflow-scrolling:touch; }
  #transcript { white-space: pre-wrap; word-break: break-word; font-size: 14px; }
  #transcript div { padding: 2px 0; }
  .stream { color: #c8c8d0; font-style: italic; }
  .empty { color: var(--dim); font-style: italic; padding: 8px 2px; }
  .task { padding:4px 2px; display:flex; gap:8px; align-items:baseline; }
  .task .g { color: var(--dim); }
  .task.in_progress .g { color: var(--acc); }
  .task.done .g { color: var(--ok); }
  .task.done { color: var(--dim); text-decoration: line-through; }
  .agent { border:1px solid #2a2a33; border-radius:8px; padding:8px 10px; margin:6px 0; }
  .agent.done { opacity:.7; }
  .agent .ah { font-weight:600; color:var(--acc); font-size:13px; }
  .agent .at { color:var(--ink); font-size:13px; margin:2px 0; }
  .agent .al { color:var(--dim); font-size:12px; white-space:pre-wrap; word-break:break-word; }
  .actions:empty { display:none; }
  .queued { color: var(--dim); font-size: 12px; padding: 2px 2px 4px; }
  .prompt { background: var(--panel); border-radius: 8px; padding: 8px 10px; margin: 6px 0; border: 1px solid var(--acc); }
  .prompt .q { font-weight: 700; color: var(--acc); }
  .opts { display:flex; flex-direction:column; gap:6px; margin:6px 0; }
  .opt { text-align:left; background:#23232b; color:var(--ink); border:1px solid #33333c; border-radius:8px;
    padding:10px 12px; cursor:pointer; display:flex; flex-direction:column; }
  .opt b { color:var(--acc); }
  .opt span { color:var(--dim); font-size:12px; }
  .chips { display:flex; gap:6px; flex-wrap:wrap; margin:6px 0 0; }
  .chip { background:var(--panel); color:var(--ink); border:1px solid #33333c; border-radius:14px;
    padding:6px 12px; font-size:13px; cursor:pointer; }
  .chip.stop { color:var(--no); border-color:#52323a; }
  .bar { display: flex; gap: 8px; margin: 8px 0 4px; flex-wrap: wrap; }
  button, input[type=text], .btn { font: inherit; border: none; border-radius: 8px; padding: 11px 16px; cursor: pointer; }
  input[type=text] { flex: 1 1 200px; background: var(--panel); color: #fff; border: 1px solid #33333c; min-width: 0; }
  input[type=text]:focus { outline: 2px solid var(--acc); }
  .send { background: var(--acc); color: #1c1c22; font-weight: 700; }
  .y { background: var(--ok); color: #1c1c22; font-weight: 700; }
  .n { background: var(--no); color: #1c1c22; font-weight: 700; }
  .conn { font-size: 12px; color: var(--dim); text-align: center; padding: 2px 0 4px; }
  footer { font-size: 11px; color: #4a4a54; text-align: center; padding: 4px 0 2px; }
</style>
</head>
<body>
<div id="banner"></div>
<header>
  <h1>⚒ Forge</h1>
  <button class="iconbtn" id="bell" title="notifications">🔕</button>
</header>
<div class="meta">
  <span class="badge" id="expo">—</span>
  <span id="cwd">—</span>
  <span>· session <span id="sid">—</span></span>
</div>
<div class="status"><span class="dot" id="dot"></span>
  <span class="tier" id="tier">—</span><span id="model">—</span>
  <span class="cost" id="cost">$0.0000</span><span class="ctx" id="ctx"></span>
  <span class="ctx" id="temper"></span></div>
<div class="tabs">
  <button class="tab on" data-tab="chat" id="tab-chat">Chat</button>
  <button class="tab" data-tab="tasks" id="tab-tasks">Tasks <span class="n" id="tc"></span></button>
  <button class="tab" data-tab="agents" id="tab-agents">Agents <span class="n" id="ac"></span></button>
</div>
<div class="panel" id="transcript"></div>
<div class="panel" id="tasks" hidden></div>
<div class="panel" id="agents" hidden></div>
<div class="actions" id="actions"></div>
<div class="chips">
  <button class="chip stop" id="stop">⏹ Stop</button>
  <button class="chip" onclick="chip('/plan')">/plan</button>
  <button class="chip" onclick="chip('/compact')">/compact</button>
  <button class="chip" onclick="chip('/diff')">/diff</button>
  <button class="chip" onclick="chip('/model')">/model</button>
</div>
<div class="bar">
  <input type="text" id="prompt" placeholder="type a task or /command…" autocomplete="off"
    autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="send">
  <button class="send" id="send">Send</button>
</div>
<div class="conn" id="conn">connecting…</div>
<footer>Forge remote control · turn off with <code>/remote</code> in the TUI</footer>

<script>
const BASE = "__BASE__";
const PROTO = 2;
const $ = (id) => document.getElementById(id);
let ws = null, dead = false, sent = 0, notif = false;
let prev = { busy:false, prompt:false, question:false };

function connect() {
  if (dead) return;
  const scheme = location.protocol === "https:" ? "wss://" : "ws://";
  ws = new WebSocket(scheme + location.host + BASE + "/ws");
  ws.onopen = () => { $("conn").textContent = "● connected"; };
  ws.onmessage = (e) => {
    let s; try { s = JSON.parse(e.data); } catch { return; }
    render(s);
    if (s.closed) { dead = true; $("conn").textContent = "remote control turned off — reconnect to the TUI"; ws.close(); }
  };
  ws.onclose = () => { if (dead) return; $("conn").textContent = "reconnecting…"; setTimeout(connect, 1500); };
  ws.onerror = () => ws.close();
}
function send(obj) { if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj)); }

function render(s) {
  if (s.protocol && s.protocol !== PROTO) {
    $("banner").style.display = "block";
    $("banner").textContent = "A newer Forge is running — refresh this page to update the remote UI.";
  }
  $("dot").className = "dot" + (s.busy ? " busy" : "");
  $("tier").textContent = s.tier ? "[" + s.tier + "]" : "—";
  $("model").textContent = s.model || "—";
  $("cost").textContent = "$" + (s.cost_usd || 0).toFixed(4);
  $("temper").textContent = s.temper ? "◆ " + s.temper : "";
  $("sid").textContent = (s.session_id || "").slice(0, 8) || "—";
  $("cwd").textContent = baseName(s.cwd) || "—";
  $("expo").textContent = s.exposure || "—";
  $("expo").className = "badge" + ((s.exposure || "").indexOf("public") === 0 ? " pub" : "");
  if (s.context_tokens > 0) {
    const lim = s.context_limit ? "/" + fmt(s.context_limit) : "";
    $("ctx").textContent = "◷ " + fmt(s.context_tokens) + lim;
  } else { $("ctx").textContent = ""; }

  renderTranscript(s);
  renderTasks(s);
  renderAgents(s);
  renderActions(s);
  notifyTransitions(s);
}

function renderTranscript(s) {
  const t = $("transcript");
  const body = (s.transcript || []).join("\n") + (s.streaming ? "\n" + s.streaming : "");
  if (t._n === sent + body.length) return; // unchanged
  const nearBottom = t.scrollHeight - t.scrollTop - t.clientHeight < 80;
  t.innerHTML = "";
  (s.transcript || []).forEach(line => { const d = document.createElement("div"); d.textContent = line; t.appendChild(d); });
  if (s.streaming) { const d = document.createElement("div"); d.className = "stream"; d.textContent = s.streaming; t.appendChild(d); }
  if (nearBottom) t.scrollTop = t.scrollHeight;
  t._n = sent + body.length;
}

// Rebuild `el`'s contents via `fill`, but preserve scroll position across the rebuild — a plain
// `innerHTML = ""` resets scrollTop to 0, which would yank the view back to the top every time a
// new snapshot arrives (e.g. a subagent's `last` line updating mid-stream) while someone is
// scrolled up reading earlier entries. Skips the rebuild entirely when `sig` is unchanged.
function rebuildPreservingScroll(el, sig, fill) {
  if (el._sig === sig) return;
  el._sig = sig;
  const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  const scrollTop = el.scrollTop;
  fill();
  el.scrollTop = nearBottom ? el.scrollHeight : scrollTop;
}

function renderTasks(s) {
  const tasks = s.tasks || [];
  $("tc").textContent = tasks.length ? tasks.filter(x => x.status === "done").length + "/" + tasks.length : "";
  const el = $("tasks");
  rebuildPreservingScroll(el, JSON.stringify(tasks), () => {
    if (!tasks.length) { el.innerHTML = '<div class="empty">no tasks yet</div>'; return; }
    el.innerHTML = "";
    tasks.forEach(t => {
      const d = document.createElement("div"); d.className = "task " + t.status;
      const g = t.status === "done" ? "●" : (t.status === "in_progress" ? "◐" : "○");
      d.innerHTML = '<span class="g">' + g + '</span><span>' + esc(t.title) + '</span>';
      el.appendChild(d);
    });
  });
}

function renderAgents(s) {
  const subs = s.subagents || [];
  $("ac").textContent = subs.length ? "" + subs.length : "";
  const el = $("agents");
  rebuildPreservingScroll(el, JSON.stringify(subs), () => {
    if (!subs.length) { el.innerHTML = '<div class="empty">no subagents running</div>'; return; }
    el.innerHTML = "";
    subs.forEach(a => {
      const d = document.createElement("div"); d.className = "agent" + (a.done ? " done" : "");
      const head = esc(a.agent || "agent") + (a.model ? " · " + esc(a.model) : "") + (a.done ? " · done $" + (a.cost || 0).toFixed(4) : "");
      d.innerHTML = '<div class="ah">' + (a.done ? "✓ " : "▸ ") + head + '</div>' +
        '<div class="at">' + esc(a.task || "") + '</div>' +
        '<div class="al">' + esc(a.last || "") + '</div>';
      el.appendChild(d);
    });
  });
}

function renderActions(s) {
  const a = $("actions");
  const queued = s.queued || [];
  let h = queued.length
    ? '<div class="queued">' + queued.map(q => "⏳ queued: " + esc(q)).join("<br>") + '</div>'
    : "";
  if (s.permission_prompt) {
    h += '<div class="prompt"><span class="q">⚠ ' + esc(s.permission_prompt) +
      '</span></div><div class="bar"><button class="y" onclick="answer(true)">Allow</button>' +
      '<button class="n" onclick="answer(false)">Deny</button></div>';
  } else if (s.question) {
    h += '<div class="prompt"><span class="q">❓ ' + esc(s.question) + '</span></div>';
    const opts = s.question_options || [];
    if (opts.length) {
      h += '<div class="opts">';
      opts.forEach((o, i) => {
        h += '<button class="opt" onclick="pick(' + (i + 1) + ')"><b>' + esc(o.label) + '</b>' +
          (o.description ? '<span>' + esc(o.description) + '</span>' : '') + '</button>';
      });
      h += '</div>';
    }
    if (!opts.length || s.question_allow_other) {
      h += '<div class="bar"><input type="text" id="ans" placeholder="answer…" enterkeyhint="done">' +
        '<button class="send" onclick="sendAnswer()">Answer</button></div>';
    }
  }
  a.innerHTML = h;
}

function notifyTransitions(s) {
  const pPrompt = !!s.permission_prompt, pQuestion = !!s.question;
  if (pPrompt && !prev.prompt) maybeNotify("Forge needs permission", s.permission_prompt);
  if (pQuestion && !prev.question) maybeNotify("Forge has a question", s.question);
  if (!s.busy && prev.busy && !pPrompt && !pQuestion) maybeNotify("Forge — turn complete", lastLine(s));
  prev = { busy: !!s.busy, prompt: pPrompt, question: pQuestion };
}
function lastLine(s) { const t = s.transcript || []; return t.length ? t[t.length - 1] : ""; }

function fmt(n) { if (n >= 1e6) return (n/1e6).toFixed(1)+"M"; if (n >= 1e3) return (n/1e3).toFixed(1)+"k"; return ""+n; }
function esc(s) { return (s||"").replace(/[&<>]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;"}[c])); }
function baseName(p) { if (!p) return ""; const parts = (""+p).replace(/[\\/]+$/, "").split(/[\\/]/); return parts[parts.length-1] || p; }

function submit() { const v = $("prompt").value; if (!v.trim()) return; send({kind:"prompt", text:v}); $("prompt").value=""; sent++; }
function chip(cmd) { send({kind:"prompt", text:cmd}); }
$("send").onclick = submit;
$("prompt").addEventListener("keydown", e => { if (e.key === "Enter") { e.preventDefault(); submit(); } });
$("stop").onclick = () => send({kind:"interrupt"});
window.answer = (yes) => send({kind:"allow", yes:!!yes});
window.pick = (n) => send({kind:"answer", text:""+n});
window.sendAnswer = () => { const v = $("ans").value; if (v.trim()) { send({kind:"answer", text:v}); sent++; } };
window.chip = chip;

// Tabs
document.querySelectorAll(".tab").forEach(b => b.onclick = () => {
  document.querySelectorAll(".tab").forEach(x => x.classList.remove("on"));
  b.classList.add("on");
  const which = b.dataset.tab;
  $("transcript").hidden = which !== "chat";
  $("tasks").hidden = which !== "tasks";
  $("agents").hidden = which !== "agents";
});

// Notifications (live, while the page/PWA is open in the background)
function paintBell() { $("bell").textContent = notif ? "🔔" : "🔕"; }
$("bell").onclick = () => {
  if (!("Notification" in window)) { $("bell").title = "notifications unsupported"; return; }
  if (Notification.permission === "granted") { notif = !notif; paintBell(); return; }
  Notification.requestPermission().then(p => { notif = (p === "granted"); paintBell(); });
};
function maybeNotify(title, body) {
  if (notif && document.hidden && "Notification" in window && Notification.permission === "granted") {
    try { new Notification(title, { body: (body||"").slice(0, 120), icon: BASE + "/icon.svg", tag: "forge-remote" }); } catch (e) {}
  }
}

// PWA: register the token-scoped service worker so the page installs to a home screen.
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register(BASE + "/sw.js", { scope: BASE + "/" }).catch(() => {});
}

$("prompt").focus();
connect();
</script>
</body>
</html>"##;

/// The token-scoped PWA service worker. Its presence + a `fetch` handler is what makes the control
/// page installable to a phone home screen; it caches the shell (network-first) so a reconnect is
/// instant. Live state flows over the WebSocket, never `fetch`, so there's nothing else to cache.
const SERVICE_WORKER: &str = r#"const CACHE = "forge-remote-v2";
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (e) => {
  const req = e.request;
  if (req.method !== "GET") return;
  e.respondWith(
    fetch(req).then((res) => {
      const copy = res.clone();
      caches.open(CACHE).then((c) => c.put(req, copy)).catch(() => {});
      return res;
    }).catch(() => caches.match(req))
  );
});
"#;

/// The app icon (inline SVG — no binary asset to serve). A hammer mark on the brand background;
/// `sizes:"any"` in the manifest lets the single SVG satisfy every install target.
const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect width="24" height="24" rx="5" fill="#16161c"/><g fill="none" stroke="#ff913c" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="m15 12-8.5 8.5c-.83.83-2.17.83-3 0a2.12 2.12 0 0 1 0-3L12 9"/><path d="M17.64 15 22 10.64"/><path d="m20.91 11.7-1.25-1.25c-.6-.6-.93-1.4-.93-2.25v-.86L16.01 4.6a5.56 5.56 0 0 0-3.94-1.64H9l.92.82A6.18 6.18 0 0 1 12 8.4v1.56l2 2h2.47l2.26 1.91"/></g></svg>"##;

/// Build the PWA manifest JSON for a token base path (e.g. `/<token>`). `start_url`/`scope` use
/// the slashed form so the installed app launches inside the service-worker scope and runs
/// standalone (no browser chrome) straight into this session's control page.
fn manifest_json(base: &str) -> String {
    format!(
        r##"{{"name":"Forge remote control","short_name":"Forge","description":"Drive a Forge coding session from anywhere.","start_url":"{base}/","scope":"{base}/","display":"standalone","background_color":"#16161c","theme_color":"#16161c","orientation":"any","icons":[{{"src":"{base}/icon.svg","sizes":"any","type":"image/svg+xml","purpose":"any"}},{{"src":"{base}/icon.svg","sizes":"any","type":"image/svg+xml","purpose":"maskable"}}]}}"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_to_json_with_all_fields() {
        let s = Snapshot {
            session_id: "abc12345".into(),
            cwd: "/home/u/proj".into(),
            exposure: "LAN".into(),
            busy: true,
            temper: "Ask".into(),
            tier: Some("complex".into()),
            model: "groq::llama-3.3-70b".into(),
            cost_usd: 0.0123,
            context_tokens: 18_200,
            context_limit: Some(200_000),
            streaming: "thinking…".into(),
            transcript: vec!["you: hi".into(), "forge: hello".into()],
            tasks: vec![SnapTask {
                title: "build it".into(),
                status: "in_progress".into(),
            }],
            subagents: vec![SnapSubagent {
                agent: "general".into(),
                task: "scan".into(),
                model: Some("haiku".into()),
                last: "reading…".into(),
                done: false,
                cost: 0.001,
            }],
            queued: vec!["next thing".into()],
            permission_prompt: Some("allow write_file".into()),
            question: None,
            question_options: vec![SnapOption {
                label: "Yes".into(),
                description: "do it".into(),
            }],
            question_allow_other: true,
            ..Default::default()
        };
        // Snapshot is server→client (serialize only); confirm the wire shape carries every field
        // the control page's JS reads, so a schema drift is caught here rather than at runtime.
        let json = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["protocol"], PROTOCOL_VERSION);
        assert_eq!(v["session_id"], "abc12345");
        assert_eq!(v["cwd"], "/home/u/proj");
        assert_eq!(v["exposure"], "LAN");
        assert_eq!(v["busy"], true);
        assert_eq!(v["tier"], "complex");
        assert_eq!(v["model"], "groq::llama-3.3-70b");
        assert_eq!(v["cost_usd"], 0.0123);
        assert_eq!(v["context_tokens"], 18200);
        assert_eq!(v["context_limit"], 200000);
        assert_eq!(v["transcript"][0], "you: hi");
        assert_eq!(v["tasks"][0]["title"], "build it");
        assert_eq!(v["tasks"][0]["status"], "in_progress");
        assert_eq!(v["subagents"][0]["agent"], "general");
        assert_eq!(v["subagents"][0]["done"], false);
        assert_eq!(v["queued"][0], "next thing");
        assert_eq!(v["permission_prompt"], "allow write_file");
        assert_eq!(v["question"], serde_json::Value::Null);
        assert_eq!(v["question_options"][0]["label"], "Yes");
        assert_eq!(v["question_allow_other"], true);
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
    fn manifest_is_token_scoped_valid_json() {
        let m = manifest_json("/deadbeef");
        let v: serde_json::Value = serde_json::from_str(&m).expect("manifest is valid JSON");
        assert_eq!(v["start_url"], "/deadbeef/");
        assert_eq!(v["scope"], "/deadbeef/");
        assert_eq!(v["display"], "standalone");
        assert_eq!(v["icons"][0]["src"], "/deadbeef/icon.svg");
    }

    #[test]
    fn control_page_injects_base_path() {
        let html = CONTROL_PAGE.replace("__BASE__", "/cafef00d");
        assert!(!html.contains("__BASE__"), "all base placeholders replaced");
        assert!(
            html.contains(r#"const BASE = "/cafef00d";"#),
            "JS BASE is the token path (WS + SW + manifest derive from it)"
        );
        assert!(
            html.contains(r#"href="/cafef00d/manifest.webmanifest""#),
            "manifest link is token-scoped"
        );
        assert!(
            html.contains(r#"href="/cafef00d/icon.svg""#),
            "icon link is token-scoped"
        );
    }

    #[test]
    fn service_worker_has_fetch_handler() {
        // PWA installability requires a fetch handler; guard against accidentally dropping it.
        assert!(SERVICE_WORKER.contains(r#"addEventListener("fetch""#));
    }

    #[test]
    fn remote_auto_maps_to_exposure() {
        use forge_config::RemoteAuto;
        assert_eq!(Exposure::from(RemoteAuto::Local), Exposure::Local);
        assert_eq!(Exposure::from(RemoteAuto::Lan), Exposure::Lan);
        assert_eq!(Exposure::from(RemoteAuto::Anywhere), Exposure::Anywhere);
        // Off never reaches `From` in practice (startup_exposure returns None), but map it safely.
        assert_eq!(Exposure::from(RemoteAuto::Off), Exposure::Local);
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
    fn cloudflared_url_parses_from_a_log_line() {
        // cloudflared prints the quick-tunnel URL in a boxed stderr log line.
        let line = "2026-06-18T17:00:00Z INF |  https://random-words-here.trycloudflare.com  |";
        assert_eq!(
            TunnelKind::Cloudflared.parse_url(line).as_deref(),
            Some("https://random-words-here.trycloudflare.com")
        );
        // A non-URL log line yields nothing.
        assert_eq!(
            TunnelKind::Cloudflared.parse_url("INF Starting tunnel"),
            None
        );
    }

    #[test]
    fn ngrok_url_parses_from_forwarding_line() {
        let line = "Forwarding   https://abc123.ngrok-free.app -> http://localhost:8080";
        assert_eq!(
            TunnelKind::Ngrok.parse_url(line).as_deref(),
            Some("https://abc123.ngrok-free.app")
        );
        // Legacy ngrok.io domain still matches.
        assert_eq!(
            TunnelKind::Ngrok
                .parse_url("Forwarding https://x.ngrok.io -> localhost")
                .as_deref(),
            Some("https://x.ngrok.io")
        );
    }

    #[test]
    fn bore_url_parses_to_an_http_address() {
        // bore logs `listening at bore.pub:<port>`; it's plain TCP, so the connect URL is http://.
        let line = "2026-06-18 INFO bore_cli::client: listening at bore.pub:40123";
        assert_eq!(
            TunnelKind::Bore.parse_url(line).as_deref(),
            Some("http://bore.pub:40123")
        );
        assert_eq!(TunnelKind::Bore.parse_url("connecting…"), None);
    }

    #[test]
    fn tunnel_argv_points_at_the_local_port() {
        assert_eq!(
            TunnelKind::Cloudflared.argv(8080),
            vec!["tunnel", "--url", "http://localhost:8080"]
        );
        assert_eq!(TunnelKind::Ngrok.argv(8080), vec!["http", "8080"]);
        assert_eq!(
            TunnelKind::Bore.argv(8080),
            vec!["local", "8080", "--to", "bore.pub"]
        );
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
            let rc = start(Exposure::Local).expect("start loopback server");
            let port = rc.url.addr.port();
            let token = rc.url.token.clone();

            // 1. The control page is served at the token URL.
            let http = forge_provider::bundled_http_client();
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

            // 4. PWA assets are served + token-scoped (so the page installs to a home screen).
            let man = http
                .get(format!(
                    "http://127.0.0.1:{port}/{token}/manifest.webmanifest"
                ))
                .send()
                .await
                .expect("GET manifest");
            assert_eq!(man.status(), 200, "manifest is 200");
            let man_body = man.text().await.unwrap();
            assert!(
                man_body.contains(&format!("\"start_url\":\"/{token}/\"")),
                "manifest start_url is token-scoped: {man_body}"
            );
            let sw = http
                .get(format!("http://127.0.0.1:{port}/{token}/sw.js"))
                .send()
                .await
                .expect("GET service worker");
            assert_eq!(sw.status(), 200, "service worker is 200");
            let icon = http
                .get(format!("http://127.0.0.1:{port}/{token}/icon.svg"))
                .send()
                .await
                .expect("GET icon");
            assert_eq!(icon.status(), 200, "icon is 200");
            // All assertions passed — force-exit so the lingering server task + WS close
            // handshake can't stall the test runtime's shutdown (manual-only, --ignored).
            std::process::exit(0);
        })
        .await;
        // Unreachable on success (exit above); only reached if the 5s timeout elapsed.
        let _ = _outcome;
        panic!("WS round-trip did not complete within 5s");
    }

    // -----------------------------------------------------------------------
    // TLS: cert generation + fingerprint
    // -----------------------------------------------------------------------

    #[test]
    fn self_signed_cert_generates_and_fingerprint_is_stable() {
        // generate_self_signed should succeed for any non-empty SAN list.
        let sans = vec!["192.168.1.10".to_string(), "localhost".to_string()];
        let cert = generate_self_signed(sans).expect("cert generation must not fail");

        // PEM blobs are non-empty and begin with the expected PEM headers.
        assert!(
            cert.cert_pem.starts_with(b"-----BEGIN CERTIFICATE-----"),
            "cert_pem must be PEM-encoded: {:?}",
            String::from_utf8_lossy(&cert.cert_pem[..40.min(cert.cert_pem.len())])
        );
        assert!(
            cert.key_pem.starts_with(b"-----BEGIN PRIVATE KEY-----")
                || cert.key_pem.starts_with(b"-----BEGIN EC PRIVATE KEY-----"),
            "key_pem must be PEM-encoded: {:?}",
            String::from_utf8_lossy(&cert.key_pem[..40.min(cert.key_pem.len())])
        );

        // Fingerprint: 64 hex digits + 31 colons = 95 chars (32 bytes × "XX:" minus trailing colon).
        // i.e. "XX:XX:…:XX" = 32 groups of 2 hex digits separated by `:` → length = 32*2 + 31 = 95.
        assert_eq!(
            cert.fingerprint.len(),
            95,
            "SHA-256 fingerprint must be 95 chars: {:?}",
            cert.fingerprint
        );
        // All non-colon chars must be uppercase hex digits.
        assert!(
            cert.fingerprint
                .chars()
                .all(|c| c == ':' || c.is_ascii_hexdigit()),
            "fingerprint chars must be hex or colon: {:?}",
            cert.fingerprint
        );
        // Colons only at positions 2, 5, 8, …
        let parts: Vec<&str> = cert.fingerprint.split(':').collect();
        assert_eq!(
            parts.len(),
            32,
            "fingerprint must have 32 colon-separated groups"
        );
        for part in &parts {
            assert_eq!(
                part.len(),
                2,
                "each group must be exactly 2 hex digits: {part:?}"
            );
            assert!(
                part.chars().all(|c| c.is_ascii_hexdigit()),
                "group must be uppercase hex: {part:?}"
            );
        }

        // Generating the same cert twice produces different fingerprints (each call generates
        // a fresh key + cert, so the DER is different even for the same SANs).
        let cert2 =
            generate_self_signed(vec!["localhost".to_string()]).expect("second cert generation");
        // It's technically possible (but astronomically unlikely) for two random certs to share
        // the same fingerprint. If this ever fires, something is wrong.
        assert_ne!(
            cert.fingerprint, cert2.fingerprint,
            "two separately generated certs must have different fingerprints"
        );
    }

    #[test]
    fn sha256_fingerprint_known_vector() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        // Verify our inline implementation against this well-known test vector.
        let empty_digest = sha256_raw(&[]);
        let expected: [u8; 32] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(
            empty_digest, expected,
            "SHA-256 of empty input must match FIPS vector"
        );

        // SHA-256("abc") — verified against Python hashlib.sha256(b"abc").digest():
        // ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        let abc_digest = sha256_raw(b"abc");
        let abc_expected: [u8; 32] = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(
            abc_digest, abc_expected,
            "SHA-256 of 'abc' must match reference vector"
        );
    }

    #[test]
    fn fingerprint_format_is_colon_separated_uppercase_hex() {
        // Feed a known 32-byte value; verify the formatted fingerprint string.
        let input = [0xABu8; 32]; // all 0xAB bytes
        let fp = sha256_fingerprint(&input);
        // sha256_fingerprint of [0xAB; 32] — the actual digest. What matters here is the FORMAT:
        // we reuse sha256_fingerprint on a real digest of a simple value.
        // Instead, directly test the format rules on the sha256_raw output.
        let digest = sha256_raw(&[0x00]);
        let formatted = sha256_fingerprint(&[0x00]);
        // Must be 95 chars: 32 groups of 2 hex digits separated by ':'
        assert_eq!(formatted.len(), 95);
        let parts: Vec<&str> = formatted.split(':').collect();
        assert_eq!(parts.len(), 32);
        // All uppercase.
        assert_eq!(formatted, formatted.to_uppercase());
        // Recompute manually from the raw digest and confirm they match.
        let expected: String = digest
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(formatted, expected);
        // Suppress unused-variable warning for the `fp` variable above.
        let _ = fp;
    }

    // Ignored: start() binds a real socket + spawns a never-ending accept loop the test runtime
    // can't reliably abort on drop, so it hangs in CI. Cert/fingerprint correctness is covered by
    // the pure tests above; serving by start_serves_page_and_upgrades_websocket. Run with --ignored.
    #[ignore = "binds + serves a real socket; hangs under the test runtime — see comment"]
    #[tokio::test]
    async fn lan_start_url_is_https_with_fingerprint() {
        // `start(Exposure::Lan)` must return an https:// URL and a populated tls_fingerprint.
        // Requires a Tokio runtime because axum-server's from_tcp_rustls wires into the runtime.
        let rc = start(Exposure::Lan).expect("start LAN server");
        assert!(
            rc.url.url.starts_with("https://"),
            "LAN URL must be https://: {}",
            rc.url.url
        );
        assert!(
            rc.url.tls_fingerprint.is_some(),
            "LAN RemoteUrl must carry a TLS fingerprint"
        );
        let fp = rc.url.tls_fingerprint.clone().unwrap();
        assert_eq!(fp.len(), 95, "fingerprint must be 95 chars: {fp}");
    }

    #[ignore = "binds + serves a real socket; hangs under the test runtime — see comment above"]
    #[tokio::test]
    async fn local_start_url_is_http_no_fingerprint() {
        // `start(Exposure::Local)` must stay plain HTTP with no fingerprint.
        // Requires a Tokio runtime because axum::serve wires into the runtime.
        let rc = start(Exposure::Local).expect("start loopback server");
        assert!(
            rc.url.url.starts_with("http://"),
            "loopback URL must be http://: {}",
            rc.url.url
        );
        assert!(
            rc.url.tls_fingerprint.is_none(),
            "loopback RemoteUrl must have no TLS fingerprint"
        );
    }
}
