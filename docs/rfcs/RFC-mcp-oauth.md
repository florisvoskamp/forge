# RFC: OAuth 2.0 authentication for the MCP client

| Field | Value |
|-------|-------|
| Status | SHIPPED |
| Author | Floris Voskamp (drafted autonomously) |
| Created | 2026-06-16 |
| Last updated | 2026-06-16 |
| Reviewers | — |
| Decision due | — |
| Implements | — |
| Supersedes | — |

---

## Summary

Forge's MCP client only authenticates HTTP servers with a **static bearer token**. OAuth-protected
servers (e.g. `helm.adulari.dev`, which advertises RFC 9728 Protected Resource Metadata) hand out
**short-lived** tokens, so the static value imported from `~/.claude.json` goes stale and the server
returns `invalid_token`. This RFC proposes first-class **OAuth 2.0 (Authorization Code + PKCE,
RFC 8252 native-app loopback)** support: a `forge mcp login <server>` browser flow, automatic
metadata discovery (RFC 9728 + RFC 8414), optional dynamic client registration (RFC 7591),
keyring-stored access/refresh tokens (ADR-0007), and automatic refresh at connect time. Outcome:
OAuth MCP servers connect and stay connected without manual token copying.

---

## Problem statement

`crates/forge-config/src/mcp.rs::resolve_token` resolves an HTTP server's bearer from an env var or
the keyring (`McpAuth { token_env, token_keyring, header }`); `crates/forge-mcp/src/transport.rs`
attaches it via `StreamableHttpClientTransportConfig::auth_header`. This works **only for tokens
that don't expire**.

The `helm` server is OAuth-protected. Its 401 carries:

```
WWW-Authenticate: Bearer error="invalid_token",
  error_description="Authentication required",
  resource_metadata="https://helm.adulari.dev/.well-known/oauth-protected-resource/mcp"
```

Today the only way to use it is to copy the bearer that Claude Code obtained into Forge (via
`forge mcp import`, which reads `~/.claude.json`'s `headers.Authorization`). That token expires
server-side within hours; once it does, helm fails with `Auth required` and the user must re-import.
This was observed twice in one day. **Impact:** the headline "connect to your MCP servers" promise
silently breaks for any OAuth server — an increasingly common class (the MCP spec adopted OAuth as
the standard remote-server auth in 2025; claude.ai connectors, Stitch/Google, and many SaaS MCPs use
it). There is no static token to copy for a pure-OAuth server at all.

### Non-goals

- **OAuth for stdio servers** — stdio servers authenticate via their own spawned process/env; no
  change.
- **Acting as an OAuth *authorization server*** — Forge is only a client.
- **Device Authorization Grant (RFC 8628)** — deferred; the loopback redirect flow covers desktop.
  A device-code fallback for headless hosts is noted as future work.
- **Implicit / password grants** — never; deprecated and insecure.
- **Sharing tokens with the CLI-bridge sub-process** beyond what already flows (the bridge connects
  its own `McpManager` in `mcp-serve`; it reuses the same keyring entries, so it benefits for free).

---

## Background and context

Relevant existing code:

- **Config + secrets (forge-config):** `McpServerConfig { name, transport, auth: Option<McpAuth>,
  enabled }`; `McpTransport::Http { url, headers }`; `McpAuth { token_env, token_keyring, header }`;
  `resolve_token(&McpAuth) -> Option<String>` (env → keyring). Secrets live in the OS keyring under
  service `"forge"` (ADR-0007: secrets NEVER in config/logs). `store_secret(key, val)` exists.
- **Transport (forge-mcp):** `transport::serve(server)` builds the rmcp `StreamableHttpClient`
  transport, attaching a bearer (`auth_header`) or a custom header from the resolved token, then
  runs the MCP `initialize` handshake. `McpManager::connect_all` connects every configured server
  at startup and records per-server status (`connected` / `failed` with a reason).
- **Import:** `forge mcp import` parses `.mcp.json` / `~/.claude.json`, extracting `Authorization`
  headers into the keyring as `mcp:<server>` and writing an `McpAuth { token_keyring }` reference.
- **ADR-0009:** MCP tool calls are `SideEffect::External`, gated by the permission broker.

OAuth for MCP follows the MCP Authorization spec, which composes standard RFCs:
- **RFC 9728** — Protected Resource Metadata: the 401's `resource_metadata` URL returns JSON naming
  the server's `authorization_servers`.
- **RFC 8414** — Authorization Server Metadata: `<issuer>/.well-known/oauth-authorization-server`
  returns `authorization_endpoint`, `token_endpoint`, `registration_endpoint`, etc.
- **RFC 7636 (PKCE)** — code-challenge so a public client needs no secret.
- **RFC 8252** — native apps use a **loopback** redirect (`http://127.0.0.1:<random>/callback`).
- **RFC 7591** — Dynamic Client Registration: obtain a `client_id` at runtime when the server
  supports it (most MCP servers do, since there's no manual app-registration UX).

---

## Proposed solution

### High-level design

Add an **`oauth` variant to `McpAuth`**. Authentication becomes a two-phase lifecycle:

1. **Login (interactive, rare):** `forge mcp login <server>` runs the Authorization Code + PKCE
   flow in the user's browser against a loopback redirect, then stores the resulting **access +
   refresh tokens** (and discovered `client_id`/endpoints) in the keyring.
2. **Connect (automatic, every run):** `forge-mcp` reads the stored access token; if absent or
   expired, it uses the refresh token to silently get a new one; only if refresh fails does it
   surface a clear "run `forge mcp login <server>`" error.

Crate boundaries are preserved: **forge-config** owns the config surface, token storage, and the
pure OAuth helpers (URL building, PKCE, metadata types — all sync/testable); **forge-mcp** owns the
transport, the HTTP calls (token endpoint, refresh), and the loopback listener. forge-cli wires the
`login` command and opens the browser.

```
forge mcp login helm
  └─ forge-mcp::oauth::login(server)
       1. GET <resource_metadata>            (RFC 9728)   ─┐ discovery
       2. GET <issuer>/.well-known/...        (RFC 8414)   ─┘
       3. POST <registration_endpoint>        (RFC 7591, if no client_id)
       4. open browser → <authorization_endpoint>?...PKCE...redirect=loopback
       5. loopback listener captures ?code=...&state=...
       6. POST <token_endpoint> (code + verifier) → {access, refresh, expires_in}
       7. forge-config::store_oauth_tokens(server, tokens, client_id, endpoints)

forge mcp   (connect)
  └─ transport::serve(server)
       token = oauth::valid_access_token(server)?   // refresh if expired
       attach Bearer → initialize
```

### Detailed design

#### Config surface (forge-config)

`McpAuth` becomes an enum-like struct gaining an OAuth mode. To stay backward compatible with
existing TOML (`token_env` / `token_keyring` / `header`), add an optional nested table:

```toml
[servers.auth]
# existing static-bearer fields still work unchanged:
# token_keyring = "mcp:helm"

# new: opt into OAuth
[servers.auth.oauth]
# all optional — discovered at login, persisted back for reuse:
issuer = "https://helm.adulari.dev"          # else discovered from the 401's resource_metadata
scopes = ["mcp"]                              # requested scopes (server may narrow)
client_id = "..."                             # set after dynamic registration; omit to register
```

Rust shape (serde, all new fields `#[serde(default)]` so old configs parse):

```rust
pub struct McpAuth {
    pub token_env: Option<String>,
    pub token_keyring: Option<String>,
    pub header: Option<String>,
    pub oauth: Option<OAuthConfig>,   // NEW
}

pub struct OAuthConfig {
    pub issuer: Option<String>,       // discovered if None
    pub scopes: Vec<String>,
    pub client_id: Option<String>,    // filled by dynamic registration
    pub redirect_port: Option<u16>,   // pin the loopback port if a firewall needs it; else ephemeral
}
```

Resolution precedence in the connect path: **static token (env/keyring) first** (cheap, unchanged),
then **oauth** if configured. A server is "OAuth" when `auth.oauth` is `Some`.

#### Token storage (forge-config, keyring — ADR-0007)

Per server, store a single JSON blob under keyring key `mcp-oauth:<server>` (distinct from the
static `mcp:<server>`):

```json
{
  "access_token": "...",
  "refresh_token": "...",
  "expires_at": 1750100000,          // unix seconds; we refresh ~60s early
  "token_endpoint": "https://.../token",
  "client_id": "...",
  "scopes": ["mcp"]
}
```

`forge-config` exposes `store_oauth_tokens(server, &OAuthTokens)` / `load_oauth_tokens(server) ->
Option<OAuthTokens>` / `clear_oauth_tokens(server)`. Tokens NEVER touch `mcp.toml` or logs. The
config only references that the server is OAuth (the `[oauth]` table holds discovery hints +
`client_id`, which is not a secret).

#### Discovery + login flow (forge-mcp::oauth)

`pub async fn login(server: &McpServerConfig) -> Result<OAuthTokens>`:

1. **Resolve issuer/metadata.** If `oauth.issuer` is set, fetch `<issuer>/.well-known/oauth-
   authorization-server` (RFC 8414). Otherwise probe the MCP `url`, read the 401's
   `WWW-Authenticate: ... resource_metadata=<url>`, GET that (RFC 9728), take its first
   `authorization_servers[]`, then fetch *that* issuer's RFC 8414 metadata. (Fallback to the
   `/.well-known/openid-configuration` path if 8414 404s.)
2. **Client id.** If `oauth.client_id` is set, use it. Else if metadata has a
   `registration_endpoint`, POST a minimal RFC 7591 registration
   (`{redirect_uris, token_endpoint_auth_method:"none", grant_types:["authorization_code",
   "refresh_token"], application_type:"native", client_name:"Forge"}`) and persist the returned
   `client_id`. Else error with guidance to set `client_id` manually.
3. **PKCE.** Generate `code_verifier` (43–128 chars, RFC 7636) and `code_challenge =
   base64url(sha256(verifier))`; generate a random `state`.
4. **Loopback listener.** Bind `127.0.0.1:0` (or `oauth.redirect_port`); `redirect_uri =
   http://127.0.0.1:<port>/callback` (RFC 8252 §7.3 — loopback, not a custom scheme).
5. **Authorize.** Open the system browser at `authorization_endpoint?response_type=code&
   client_id=...&redirect_uri=...&scope=...&state=...&code_challenge=...&code_challenge_method=S256`.
   Print the URL too (headless / browser-open failure fallback: user pastes it).
6. **Capture.** The listener accepts exactly one request, validates `state`, extracts `code`,
   returns a small "you can close this tab" HTML page, then shuts down (bounded by a 5-min timeout).
7. **Token exchange.** POST `token_endpoint` with `grant_type=authorization_code, code,
   redirect_uri, client_id, code_verifier`. Parse `{access_token, refresh_token, expires_in}`.
8. Persist via `store_oauth_tokens`.

`pub async fn valid_access_token(server) -> Result<String>` (connect path):
- Load tokens. If `expires_at` is >60s away, return the access token.
- Else POST `token_endpoint` `grant_type=refresh_token` → new tokens → persist → return.
- If no tokens or refresh fails (e.g. refresh token revoked/expired) → `Err(NeedsLogin)`.

#### Connect-path integration (forge-mcp::transport)

`serve()` gains, before building the transport:

```rust
let bearer = if let Some(_) = server.auth.as_ref().and_then(|a| a.oauth.as_ref()) {
    match oauth::valid_access_token(server).await {
        Ok(t) => Some(t),
        Err(NeedsLogin) => return Err(McpError::NeedsLogin(server.name.clone())), // clear status
    }
} else {
    server.token() // existing static path, unchanged
};
```

`McpManager::connect_all` maps `NeedsLogin` to a status line: `helm  needs login — run
\`forge mcp login helm\`` instead of the opaque transport error. A 401 *after* a successful
initialize (token revoked mid-session) triggers one refresh-and-retry; a second 401 → `NeedsLogin`.

#### CLI surface (forge-cli)

- `forge mcp login <server>` — runs `oauth::login`, opens the browser (cross-platform: `open` on
  macOS, `xdg-open` on Linux, `cmd /C start` on Windows via the `open`-style helper or a tiny
  internal matcher), prints progress + the URL fallback, reports success.
- `forge mcp logout <server>` — `clear_oauth_tokens`.
- `forge mcp` status already shows per-server state; OAuth servers show `needs login` when unauthed.
- `/mcp login <server>` in the TUI invokes the same path (the browser opens; the loopback listener
  runs in the Forge process).

### Data model changes

No DB/schema changes. New keyring entries `mcp-oauth:<server>`. New optional `[servers.auth.oauth]`
TOML table (additive, backward compatible).

### API changes

CLI only (above). No HTTP API. The rmcp transport is reused unchanged once a bearer is in hand.

### Migration plan

Fully additive. Existing static-bearer servers keep working (the `oauth` field defaults to `None`).
For helm specifically: add `[servers.auth.oauth]` (issuer discovered) and run `forge mcp login
helm` once; the stale static `mcp:helm` keyring entry can be cleared. `forge mcp import` can, in a
follow-up, detect a server that 401s with `resource_metadata` and scaffold the `[oauth]` table
instead of copying a doomed static token.

### Rollout plan

Behind the existing per-server config — a server only uses OAuth when `[oauth]` is present, so
there's nothing to flag-gate globally. Ship `login`/`logout` + connect-path refresh together.
Rollback = revert; static tokens are unaffected. No kill switch needed (inert unless configured).

---

## Alternatives considered

### Alternative 1: Do nothing (keep manual re-import)

**Description:** Users re-run `forge mcp import` whenever the token expires.

**Why rejected:** The token expires in hours; for a pure-OAuth server there is often **no static
token at all** to import (Claude stores it in its own credential store, not always in
`~/.claude.json`). It's not a workaround, it's a treadmill — and it silently breaks the core MCP
promise.

### Alternative 2: `token_command` — shell out to fetch a fresh token

**Description:** Add `McpAuth.token_command` that runs a user-configured shell command at connect
to print a current token (e.g. `jq` over `~/.claude.json`, or a vendor CLI).

**Why rejected as the primary solution:** Couples Forge to whatever external tool holds a live
token, only works if such a tool exists and keeps the token fresh, and runs arbitrary shell on every
connect (extra attack surface). It does **not** solve helm (the value in `~/.claude.json` is itself
stale). *However* it's cheap and genuinely useful for other setups, so it's proposed as a **small
companion feature**, not a replacement — see Open Question 1.

### Alternative 3: Read Claude Code's OAuth credential store directly

**Description:** Have Forge read the live token from Claude Code's own credential storage.

**Why rejected:** Undocumented, version-fragile, OS-keychain-specific, and a credential-exfiltration
pattern the permission classifier rightly resists. Forge should own its own OAuth, not scrape
another app's secrets.

### Alternative 4: Device Authorization Grant (RFC 8628) instead of loopback

**Description:** Show a code + URL; user authorizes on any device; Forge polls the token endpoint.

**Why rejected (for v1):** Many MCP authorization servers don't enable the device grant; loopback
is the RFC 8252 recommendation for desktop and needs no server opt-in beyond a registered
`http://127.0.0.1` redirect. Device grant is the right **headless** fallback — deferred to future
work, not v1.

---

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Loopback port blocked / in use | Medium | Medium | Bind `:0` (ephemeral); allow `redirect_port` override; clear error + URL-paste fallback |
| Authorization-code interception on loopback | Low | High | PKCE (S256) mandatory; `state` validated; listener bound to `127.0.0.1` only, accepts one request, 5-min timeout |
| Refresh token revoked/expired | Medium | Low | Connect-path detects refresh failure → `needs login` status with the exact command; no crash |
| Server lacks dynamic registration AND user sets no `client_id` | Medium | Medium | Detect missing `registration_endpoint`; error tells the user to register an app and set `client_id` |
| Metadata discovery variations (8414 vs OIDC path, non-spec servers) | Medium | Medium | Try RFC 8414 then OIDC `openid-configuration`; allow explicit `issuer`/endpoint overrides in config |
| Browser-open fails (SSH/headless) | Medium | Low | Always print the URL; future: device-grant fallback |
| Tokens leak to disk/logs | Low | High | Keyring-only (ADR-0007); never log token values; redact in errors; `mcp.toml` holds only non-secret hints |
| Clock skew → premature/late refresh | Low | Low | Refresh 60s early; treat a post-init 401 as "refresh once then re-login" |

---

## Security considerations

- **PKCE (RFC 7636, S256) is mandatory** — Forge is a public native client with no client secret;
  PKCE binds the code to this session so an intercepted code is useless.
- **Redirect validation:** loopback host `127.0.0.1` only (never `0.0.0.0`/LAN); the listener
  validates `state`, accepts exactly one callback, and times out. No custom URI scheme (avoids the
  scheme-hijack class RFC 8252 warns about).
- **Token storage:** access + refresh tokens in the OS keyring only (ADR-0007). `mcp.toml` and logs
  never contain token values; error messages redact them. `client_id` and endpoints are not secrets
  and may be persisted in config for reuse.
- **Transport:** all OAuth endpoints + the MCP server must be HTTPS (reject `http://` issuers except
  the loopback redirect itself). Uses the existing rustls stack.
- **Scope minimization:** request only configured `scopes`; record granted scopes; surface if the
  server narrows them.
- **Permission model unchanged:** OAuth only obtains the bearer; every resulting MCP *tool call* is
  still `SideEffect::External` and gated by the permission broker (ADR-0009). Auth ≠ authorization to
  run tools.
- **Dynamic registration** posts only a redirect URI + client name; no secret is created or stored
  (`token_endpoint_auth_method: none`).

---

## Operational considerations

- New keyring entries `mcp-oauth:<server>`; `forge mcp logout` clears them. On a keyring-less host
  (headless CI), OAuth login can't persist — document that OAuth servers need a desktop login, and
  that static `token_env` remains available for CI.
- `forge mcp` status gains a `needs login` state — self-explanatory, points at the command.
- No new always-on background process: the loopback listener exists only during `login`.

## Performance considerations

Negligible. Connect adds one keyring read; a refresh (only near expiry) adds one HTTPS POST. No
change to steady-state tool-call latency. Discovery + browser flow happen only during the rare
interactive `login`.

---

## Open questions

1. Ship the small `token_command` companion (Alternative 2) in the same change, or separately? It's
   ~30 lines and helps non-OAuth dynamic-token setups, but widens scope.
2. Should `forge mcp import` auto-detect an OAuth server (probe for a 401 + `resource_metadata`) and
   scaffold `[oauth]` instead of copying a static token — in this RFC's scope or a follow-up?
3. Bundle the RFC 8628 device-grant headless fallback now, or defer until a headless user needs it?
4. Persist discovered endpoints (token/auth) in the keyring blob (proposed) vs. re-discovering each
   connect? Persisting is faster + works offline-ish but can go stale if the server rotates
   endpoints; re-discovery is robust but adds latency. Proposed: persist, re-discover on failure.
5. One reusable loopback redirect port across servers vs. ephemeral per login? Ephemeral proposed;
   a pinned `redirect_port` is offered for firewalled hosts.

---

## Decision log

| Date | Decision | Rationale |
|------|----------|-----------|
| — | — | — |

---

## References

- MCP Authorization specification (OAuth 2.1 profile for MCP)
- RFC 9728 — OAuth 2.0 Protected Resource Metadata
- RFC 8414 — OAuth 2.0 Authorization Server Metadata
- RFC 8252 — OAuth 2.0 for Native Apps (loopback redirect)
- RFC 7636 — PKCE
- RFC 7591 — OAuth 2.0 Dynamic Client Registration
- RFC 8628 — Device Authorization Grant (deferred fallback)
- Code: `crates/forge-config/src/mcp.rs` (`McpAuth`, `resolve_token`), `crates/forge-mcp/src/transport.rs` (`serve`), `docs/features/mcp-client.md`, ADR-0007 (secrets), ADR-0009 (`SideEffect::External`)
- `docs/known-issues.md` — helm OAuth token expiry (the motivating bug)
