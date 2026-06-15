# Feature: Web tools (`web_search` + `web_fetch`)

Status: building (Wave 2, P1). Closes the largest capability gap by usage evidence —
WebSearch 373 + WebFetch 187 = **560** uses in the owner's history; the agent today cannot
research the web at all.

## Problem (JTBD)

> When I'm coding and hit something I don't know (an API, an error, a library version), I
> want the agent to **search the web and read a page** inline, so it can ground its answer in
> current facts instead of stale training knowledge.

Forge's tool set is read/write/edit/list/search/shell — all local. No network reach. A 2026
coding harness that can't look anything up is crippled for real work.

## Scope (MoSCoW)

- **Must:** a `web_fetch` tool (keyless: GET a URL → clean text) and a `web_search` tool
  (BYOK: query → ranked results). Both gated by a new `Network` side-effect class. SSRF guard
  on fetch. Pluggable search backend; Brave Search as the reference backend.
- **Should:** `forge auth brave` to store the search key in the OS keyring; clear actionable
  error when `web_search` runs with no key configured.
- **Could:** additional search backends (Tavily/Serper/SearXNG) behind the same trait.
- **Won't (this iteration):** rendering JS pages, crawling/recursion, a `forge init` step for
  the search key (documented follow-up), caching.

### Non-goals
This feature does not execute page JavaScript, does not crawl beyond the single fetched URL,
and does not cache responses.

## Network side effect (permission model)

Network egress is a distinct effect class from a local read (SSRF, data exfiltration, cost).
Add `SideEffect::Network`. Mode mapping (`decide_mode`):

| Mode | Network | Rationale |
|------|---------|-----------|
| `Plan` | **Deny** | read-only contract = no side effects, incl. egress |
| `Default` | **Ask** | confirm the first network call; an allow-rule pins it |
| `AcceptEdits` | **Allow** | auto-runs reads/edits; a network read is low-risk |
| `Bypass` | **Allow** | "do anything" |

The FR-10 rules engine layers on top: a user can `allow`/`deny` `web_fetch`/`web_search` by
host/query pattern regardless of mode (e.g. deny `web_fetch` to internal hosts).

## `web_fetch` (keyless)

- Args: `{ "url": string, "max_chars"?: number }` (default cap ~10000 chars of extracted text).
- GET via `reqwest` (rustls), bounded redirects, request timeout, body size cap, a Forge
  User-Agent. HTML → plain text: drop `<script>/<style>`, strip tags, decode common entities,
  collapse whitespace; prepend the `<title>` when present.
- **SSRF guard** (`is_safe_url`): scheme must be `http`/`https`; reject literal private,
  loopback, link-local, and unique-local IPs and `localhost`/`*.local` hostnames. Known limit
  (documented): no DNS resolution, so DNS-rebinding to a private IP is not caught in v1.

## `web_search` (pluggable backend; free by default)

- Args: `{ "query": string, "count"?: number }` (default 5, capped at 10).
- `SearchBackend` trait → two impls shipped:
  - **`DuckDuckGo`** — the **keyless free default**, two-stage:
    1. the no-JS HTML endpoint (`GET https://html.duckduckgo.com/html/?q=…`) for full ranked
       web results — parses `<a class="result__a">` + `result__snippet`, decodes `uddg=` hrefs;
    2. **fallback to the official Instant-Answer JSON API** (`api.duckduckgo.com?format=json`)
       when the HTML endpoint is throttled. DDG returns **HTTP 202 + a challenge page** when it
       rate-limits an IP — which is *technically* 2xx, so the first cut parsed it to an empty
       list and silently reported "No results found" (the bug). The IA API still returns an
       abstract + related topics under that throttle, so keyless search degrades gracefully.
    If both stages are empty **and** the HTML endpoint was throttled, `web_search` returns an
    actionable error (rate-limited; retry or `forge auth brave`) — never a misleading empty.
  - **`BraveSearch`** — used when a key is set. **Verified contract** (official docs, 2026-06):
    `GET https://api.search.brave.com/res/v1/web/search?q=…&count=…`, header
    `X-Subscription-Token: <key>`, results at `web.results[].{title,url,description}`.
- Backend selection (`resolve_backend`): explicit backend (tests/config) → else `BRAVE_API_KEY`
  (env or keyring `brave`, via `forge auth brave`) → else **DuckDuckGo**. So web search works
  with no setup at all.
- **Pricing note:** Brave removed its free tier early 2026 (now metered). DuckDuckGo is the
  free default; Tavily/SearXNG can slot in behind the same trait later.

## Bridge isolation — Forge's tools must be the search path

When a turn routes to a CLI bridge (`codex-cli::` / `claude-cli::`), the bridged CLI must use
*Forge's* web tools, not its own — otherwise search escapes Forge's gate/observability and uses
the user's personal config.

- **codex** previously loaded `~/.codex/config.toml`, pulling in the user's personal MCP
  servers (e.g. a `brave-search`/`filesystem` server) that codex used instead of Forge's tools.
  Fixed by `--ignore-user-config` on the codex harness invocation (auth still resolves via
  `CODEX_HOME`). Verified live: the user's MCP fleet disappears with the flag.
- **codex native web search** (`web.run`) is subscription-backend-injected and **cannot be
  hard-disabled** from Forge (`tools.web_search=false` / no `--search` don't stop it).
- **Mitigation (soft, works in practice):** a harness preamble (`HARNESS_TOOL_PREAMBLE`)
  instructs the bridged CLI to use `mcp__forge__web_search`/`web_fetch` and avoid native
  search. Verified live: codex obeys and calls Forge's tools (visible as `↳ web_search` in the
  TUI). If it ever ignores the nudge, Forge still observes the native call in the event stream.
- **claude** needs none of this — it's already locked to Forge's tools via `--tools ""` +
  `--strict-mcp-config` + `--allowedTools mcp__forge`.

## Acceptance criteria

```
AC1  Given the registry,  Then web_fetch and web_search are registered, both SideEffect::Network.
AC2  decide_mode: Network → Plan Deny, Default Ask, AcceptEdits Allow, Bypass Allow.
AC3  is_safe_url rejects http://127.0.0.1, http://localhost, http://10.0.0.1,
     http://169.254.169.254, file://…, ftp://…; accepts https://example.com.
AC4  html_to_text strips tags + script/style and decodes &amp;/&lt;/&gt;/&quot;; title is surfaced.
AC5  parse_brave_results(sample) → ordered [{title,url,description}] from web.results[].
AC6  web_search with no backend + no BRAVE_API_KEY → Err with an actionable "set a key" message.
AC7  web_search with a mock backend → formatted, numbered results (title / url / description).
```

## Impact

| Layer | File | Change |
|------|------|--------|
| Types | `forge-types` SideEffect | add `Network` variant |
| Permission | `forge-core::permission::decide_mode` | map `Network` per the table |
| Tools | `forge-tools` (new `web.rs`) | `WebFetchTool`, `WebSearchTool`, `SearchBackend`/`BraveSearch`, pure `is_safe_url`/`html_to_text`/`parse_brave_results`; register in `with_core_tools` |
| Deps | workspace + `forge-tools/Cargo.toml` | `reqwest` (rustls, json) — already in the lock via genai |
| Config | `forge-config` | `known_search_providers()` (`brave`), `inject_search_keys()`; `forge auth` accepts search providers |
| CLI | `forge-cli` | call `inject_search_keys()` in `build_session_with`; `forge auth brave` |

## Definition of done

- [ ] Both tools registered + `Network` side effect; mode mapping tested (AC1–AC2).
- [ ] SSRF guard + HTML→text + Brave parse are pure, unit-tested (AC3–AC5).
- [ ] No-key path returns an actionable error; mock-backend path renders results (AC6–AC7).
- [ ] `forge auth brave` stores to keyring; key injected before a session runs.
- [ ] clippy -D warnings + fmt clean; full workspace green.
