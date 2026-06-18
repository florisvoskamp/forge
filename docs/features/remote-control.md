# Feature: remote control — drive `forge chat` from a phone/desktop browser

> **Status (shipped):** `/remote` (alias `/rc`) slash command in the interactive TUI; an
> in-process `axum` HTTP + WebSocket server (`crates/forge-cli/src/remote.rs`) bound to
> `0.0.0.0:<ephemeral>` (LAN-reachable, default) or `127.0.0.1` (`--local`); a token-gated
> single-page control surface (self-contained HTML/CSS/JS, no framework, responsive for
> mobile + desktop); a `◉ remote` statusline segment that lights while active; a QR code
> printed into the TUI scrollback so a phone can scan-to-connect. The server reuses the
> running session's presenter channel — no second process, no IPC, no keys to configure.
>
> **Deferred:** TLS (the token travels in cleartext over the URL path; use `--local` on
> untrusted networks), a connection list / multi-client awareness, a config knob for the
> bind address + default port, and full-transcript mirroring (the phone shows a bounded tail).

> A new control surface layered onto the existing `run_chat_tui` loop. It adds *how a user
> can drive a session* (a browser anywhere on the LAN, or loopback for a single machine) and
> *how the live state is observed remotely* (a JSON `Snapshot` broadcast over a WebSocket),
> without changing the agent loop's contract or the `Presenter`/`UiMsg` seam the TUI already
> uses. The remote input path injects prompts / permission answers / interrupts exactly as
> local keystrokes do, so the permission gate, hooks, and temper all apply unchanged.

## 1. Problem (JTBD)

> When I start a long Forge run and step away from my desk, I want to keep an eye on it and
> steer it (approve a write, interrupt a runaway turn, send a follow-up) from my phone —
> without SSH, without a second app, without exposing my API keys to a web service — so the
> session isn't abandoned the moment I leave the keyboard.

Forge's interactive surface is a terminal TUI on the machine running the session. There is
no way to observe or drive it from another device. SSH + tmux works for power users but
needs a shell client on the phone and an exposed SSH port; a hosted web UI needs a relay
that sees the session. Neither is "easy and accessible for both desktop/mobile".

## 2. Design

**Transport:** a tiny `axum` server with two static, token-gated routes — `/<token>` (the
HTML control page) and `/<token>/ws` (a bidirectional WebSocket). The token is 16 hex chars
generated at start time and printed into the TUI scrollback (as a URL **and** a QR code).
A request that doesn't match either route hits a 404 fallback that doesn't reveal remote
control is running. `--lan` (default) binds `0.0.0.0` so a phone on the same network can
connect; `--local` binds loopback only.

**Live state → browser:** each dirty frame the render loop builds a `Snapshot` (busy ·
temper · tier · model · cost · context fill · the streaming reply's tail · a bounded ring
of recent scrollback lines · any pending permission prompt or question) and publishes it
on a `tokio::sync::watch` channel. The WS task forwards every change to each connected
browser, so the control page mirrors the TUI statusline + conversation edge in real time.

**Browser → session:** the page sends `RemoteInput` JSON (`{kind:"prompt",text}`,
`{kind:"allow",yes}`, `{kind:"answer",text}`, `{kind:"interrupt"}`) over the WS. The render
loop drains the input queue each iteration and injects each one through the *same* paths a
local keystroke takes — a prompt runs `spawn_turn` (respecting the busy guard + prompt
hooks), `allow` answers a pending permission, `answer` resolves an `AskUserQuestion`, and
`interrupt` aborts the turn task. The permission gate, temper, and command dispatch are all
unchanged — a remote prompt is indistinguishable from a local one.

**Toggle:** `/remote` is a builtin command (`CommandAction::Remote { lan }`) that returns
`DispatchOutcome::ToggleRemote`. The loop's `toggle_remote` helper starts the server (on)
or drops the `RemoteControl` handle (off — its `Drop` sends a `closed` snapshot so
connected browsers stop reconnecting, then aborts the server task). It's in the non-mutating
guard list, so it toggles even mid-turn. The `◉ remote` statusline segment reflects the
state at a glance.

**Why in-process + WebSocket (not a second binary, not SSE):** the session, presenter
channel, and `App` state already live in the `forge chat` process — reusing them is zero
new IPC and zero key configuration. The control surface needs to *send* input (not just
receive state), so a server→client-only SSE isn't enough; a WebSocket carries both
directions over one connection. `axum` bundles `hyper` + `tokio-tungstenite`, and the
workspace already has `reqwest` (rustls) + a multi-thread tokio runtime, so the added
dependency surface is small.

## 3. Security posture

The threat model is **a peer on the same LAN** (coffee-shop / shared Wi-Fi), not a
determined adversary with a sniffer. Defenses:

- **Token-gated paths.** A 64-bit random token in the URL path; without it a peer gets a
  404 and can't even tell remote control is on. The token is only valid while `forge chat`
  is running.
- **`--local` escape hatch.** `forge chat` then `/remote --local` binds `127.0.0.1` —
  control from this machine only, never the LAN.
- **No secrets exposed.** The server serves only the static control page + the live
  `Snapshot` (model name, cost, transcript tail, prompts). API keys never leave the
  process.

**Known gap (deferred):** the default `--lan` bind is plain HTTP, so the token travels in
cleartext and a LAN sniffer could capture it. TLS termination in-process (self-signed +
show the fingerprint in the TUI) is the follow-up. Until then, `--local` is the safe choice
on untrusted networks.

## 4. Surfaces touched

| Layer | Change |
|---|---|
| `forge-cli/src/remote.rs` (new) | Server, `Snapshot`/`RemoteInput` types, control page, QR renderer |
| `forge-tui/src/app.rs` | `App.remote_active`, `question_prompt`, `recent_transcript` ring, `drain_flush_remote`, `remote_snapshot`, `print_lines`, statusline `◉ remote` segment |
| `forge-tui/src/commands.rs` | `CommandAction::Remote { lan }`, `/remote` (alias `/rc`) parse + registry entry |
| `forge-cli/src/main.rs` | `DispatchOutcome::ToggleRemote`, `toggle_remote`, remote input draining + snapshot broadcast in `run_chat_tui` |
| `Cargo.toml` | `axum` (ws), `tokio-tungstenite`, `qrcode`; `tokio` `net` feature |
| `forge-cli/tests/tui_e2e.rs` | `tui_remote_control_toggles_and_shows_statusline_indicator` |

The stdin-prompt fix (`ef8a365`, feed CLI-bridge prompts via stdin to avoid `ARG_MAX`) is
included on this branch — it's the prior commit this feature builds on.
