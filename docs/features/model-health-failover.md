# Feature: Model health & failover

## Problem

The mesh routes each turn to the single best usable model and calls it once
(`forge-core::Session::run_turn` → `provider.complete(&decision.model …).await?`).
"Usable" today means only *a provider key is present* (`HeuristicRouter::model_available`
→ `has_api_key`). It has no idea whether the model actually answers.

Observed failure (user, 2026-06-15): auto-discovery ranked `gemini::antigravity-preview-05-2026`
#1 for the Trivial tier and routed to it. The call returned

```
HTTP 429 Too Many Requests / RESOURCE_EXHAUSTED
Quota exceeded ... limit: 0, model: antigravity ... Please retry in 37.047405996s
"retryDelay": "37s"
```

`limit: 0` means the model is **not on the free tier at all** for this key — it will
never succeed, yet the mesh keeps picking it because the key exists. The `?` in
`run_turn` propagated the error and **the whole turn failed** with a wall of JSON. No
retry, no failover, no memory: the next turn picks the same dead model again.

### JTBD

> When the model the mesh picked is rate-limited, out of quota, or down, I want Forge
> to quietly route around it to the next-best model and keep working, so a transient
> provider problem doesn't kill my turn or make me hand-edit config.

## Scope (MoSCoW)

**Must**
- Classify provider errors into *retryable* (rate-limit / quota, 5xx / unavailable,
  auth) vs *non-retryable* (bad request, context-length — fails the same on any model).
- On a retryable error mid-turn: bench the failing model, re-route to the next-best
  *healthy* model, and transparently retry the same turn. User sees a one-line
  "failing over" note, not a stack trace.
- A persisted health store (`model_health`) so a benched model stays benched across
  Forge restarts (a daily-quota-exhausted provider isn't re-tried every launch).
- Honor the server's cooldown: parse `Retry-After` header / Gemini `retryDelay` for an
  exact bench duration; fall back to a configured default.
- The router excludes benched models from both the primary pick and the fallback chain.

**Should**
- `forge models --probe`: actively ping every discovered model, store the verdict
  (clear healthy ones, bench dead ones), and print a per-model report. This is the
  user-driven "rescan" — startup never auto-probes (no latency / quota burn).
- A startup hint when models are benched: tell the user how many and how to rescan.

**Could**
- Exponential backoff on repeated benching of the same model (longer each time).

**Won't (this iteration)**
- Automatic background re-probing on a timer.
- Failing over on context-length errors by switching to a larger-context model.
- Streaming-mid-answer recovery (if a model dies after emitting partial text, we fail
  over for the *next* step; already-streamed text stays).

## Acceptance criteria

```
AC-1  Given the routed model returns HTTP 429
      When run_turn calls the provider
      Then the model is benched, a Warning "…unavailable — failing over" is emitted,
       and the turn retries on the next-best healthy model and succeeds.

AC-2  Given a 429 whose body carries "retryDelay": "37s" (or a Retry-After header)
      When the model is benched
      Then cooldown_until = now + 37s (the server value), not the default.

AC-3  Given a model was benched with cooldown_until in the future
      When the next turn routes (even after a Forge restart)
      Then that model is not chosen and does not appear in the fallback chain.

AC-4  Given a benched model whose cooldown_until has passed
      When routing
      Then it is eligible again (the snapshot only includes still-benched models).

AC-5  Given the provider returns a non-retryable error (e.g. 400 bad request)
      When run_turn calls the provider
      Then no model is benched and the turn fails as today (no pointless failover).

AC-6  Given every candidate across all tiers is benched/unavailable
      When run_turn fails over off the last one
      Then the turn ends with a clear "no healthy model available" error, not a hang.

AC-7  Given `forge models --probe`
      When a model answers a tiny ping
      Then it is cleared in model_health; a model that 429s/401s is benched with a
       reason and an appropriate cooldown; the command prints the verdict per model.

AC-8  Given benched models exist in the store
      When an interactive session starts
      Then a one-line hint shows the count and `forge models --probe` to rescan.
```

## Technical design

### Error classification (forge-provider)

`ProviderError` gains typed, classified variants (was a single `Request(String)`):

```rust
pub enum ProviderError {
    Request(String),                                   // non-retryable (4xx misuse, parse, etc.)
    RateLimited { message: String, retry_after: Option<Duration> },  // 429 / RESOURCE_EXHAUSTED
    Unavailable(String),                               // 5xx, connection/stream drop, timeout
    Auth(String),                                      // 401 / 403 — key bad/unauthorized
}
impl ProviderError {
    pub fn is_retryable(&self) -> bool;                // true for the latter three
    pub fn cooldown(&self, default: Duration) -> Duration; // retry_after if present, else default
}
```

`genai_provider` maps `genai::Error` → `ProviderError` **before** stringifying, so the
typed `StatusCode`/`HeaderMap` are used where genai exposes them:

- `HttpError { status, body }` and `WebModelCall { webc_error: ResponseFailedStatus
  { status, body, headers } }` → match on the typed `StatusCode`: 429 → `RateLimited`
  (Retry-After from `headers`, else `retryDelay` from `body`), 401/403 → `Auth`,
  5xx → `Unavailable`, else `Request`.
- `WebStream { cause, .. }` / `ChatResponse { body }` → the **streaming** path Forge
  uses; genai only gives a string/JSON here, so scan it: `429`/`RESOURCE_EXHAUSTED`/
  `quota` → `RateLimited` (+ parse `retryDelay`), auth markers → `Auth`, else
  `Unavailable` (the stream broke).
- everything else → `Request` (non-retryable).

A `parse_retry_after(headers, body) -> Option<Duration>` helper handles the
`Retry-After` header (delta-seconds) and the Gemini JSON `"retryDelay": "37s"` form.
Unit-tested against the exact 429 body from the bug report.

### Health snapshot (forge-types) + store (forge-store)

`forge-types::ModelHealth` — a cheap snapshot the router consults (no clock, no I/O):

```rust
#[derive(Debug, Default, Clone)]
pub struct ModelHealth { benched: HashSet<String> }   // models currently in cooldown
impl ModelHealth { fn is_benched(&self, model: &str) -> bool; fn is_empty(&self) -> bool; }
```

`model_health` table:

```sql
CREATE TABLE IF NOT EXISTS model_health (
    model          TEXT PRIMARY KEY,
    cooldown_until INTEGER NOT NULL,   -- epoch secs; benched while > now
    reason         TEXT NOT NULL,      -- "rate-limited (429)", "auth", "probe: quota 0", …
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
```

Store API (clock logic stays in the store, consistent with `spend_today_usd`):
- `bench_model(model, cooldown_until_epoch, reason)` — upsert.
- `clear_model_health(model)` — a healthy probe clears a bench.
- `benched_models(now_epoch) -> ModelHealth` — `WHERE cooldown_until > now`.
- `benched_report(now_epoch) -> Vec<(model, cooldown_until, reason)>` — for the hint/CLI.

### Routing (forge-mesh)

`Router::route` takes the health snapshot (mirrors how `BudgetState` is already passed):

```rust
async fn route(&self, prompt: &str, budget: BudgetState, health: &ModelHealth) -> RoutingDecision;
```

`RoutingDecision` gains an ordered, already-filtered fallback chain:

```rust
pub struct RoutingDecision { pub tier, pub model: String, pub rationale: String,
                             pub fallbacks: Vec<String> }
```

Selection change: a candidate is usable only if `model_available(m) && !health.is_benched(m)`.
`model` = best usable (capability-ranked in auto mode, cheapest in configured mode);
`fallbacks` = the remaining usable candidates for the tier, then the cross-tier usable
models (Complex→Standard→Trivial), in preference order, deduped. If *nothing* is usable
the decision keeps the original model + a warning (errors downstream, as today, but now
AC-6 turns that into a clean message).

### Turn failover loop (forge-core)

`run_turn` builds the snapshot from the store (like `BudgetState`) and passes it to the
router. The single `provider.complete(&decision.model …).await?` becomes a walk down
`[decision.model] ++ decision.fallbacks`:

```
active = decision.model
loop:
  match provider.complete(active, …).await:
    Ok(resp) => use it (price + record under `active`, which may differ from the pick)
    Err(e) if e.is_retryable():
        store.bench_model(active, now + e.cooldown(default), reason(&e))
        presenter.emit(Warning("{active} {reason} — failing over"))
        match chain.next():
          Some(next) => { presenter.emit(Routing{model: next, rationale:"failover"}); active = next; continue }
          None       => return Err(NoHealthyModel)      // AC-6
    Err(e) => return Err(e.into())                       // AC-5 non-retryable
```

The actually-used `active` model (not the original pick) is what cost, usage, and the
`routing_decision` row are recorded against, so accounting stays correct after a failover.
The loop wraps each step's `complete`, so a model that dies mid-tool-loop also fails over.

### Probe + CLI (forge-cli)

- `forge models --probe`: for each discovered model, send a 1-token ping
  (`provider.complete(model, [user "ping"], no tools)` with a short timeout); classify
  the result with the same path. Healthy → `clear_model_health`; retryable error →
  `bench_model` (cooldown from the error, or a long default for `limit: 0`/auth);
  print `✓ model` / `✗ model — reason (benched 37s)`. Bare `forge models` is unchanged
  (discovery + ranked pick) and additionally annotates currently-benched models.
- Startup hint: `build_session_with` reads `benched_report`; if non-empty, the
  TUI/plain surface prints `⚠ N model(s) benched — \`forge models --probe\` to recheck`.

### Config (forge-config)

```toml
[mesh]
failover = true               # default; false = single-shot (old behaviour)
failover_cooldown_secs = 300  # default bench when the server gives no Retry-After
```

## Impact

| Layer | Change |
|-------|--------|
| forge-provider | `ProviderError` typed variants + `is_retryable`/`cooldown`; genai error classifier + retry-after parser |
| forge-types | `ModelHealth` snapshot |
| forge-store | `model_health` table + bench/clear/benched_models/benched_report |
| forge-mesh | `Router::route` gains `&ModelHealth`; `RoutingDecision.fallbacks`; benched-aware selection + chain builder |
| forge-core | `run_turn` builds snapshot, failover loop, bench-on-error, record under active model |
| forge-cli | `--probe` flag + rescan logic; startup benched hint; thread snapshot through build |
| forge-config | `mesh.failover`, `mesh.failover_cooldown_secs` |

Ripple: `Router::route` signature change touches `HeuristicRouter`, `LlmRouter`, and all
mesh/core tests (pass `&ModelHealth::default()` = all healthy).

## Definition of done

- [ ] All ACs covered by tests.
- [ ] Retry-after parser tested against the real 429 body.
- [ ] Failover loop tested with a mock provider that errors then succeeds (AC-1) and a
      non-retryable error that does not fail over (AC-5), and full-exhaustion (AC-6).
- [ ] Bench persists across a store reopen (AC-3); expired bench is eligible (AC-4).
- [ ] `forge models --probe` benches/clears + prints; startup hint shows when benched.
- [ ] `cargo test --workspace` + `cargo clippy --workspace -- -D warnings` clean.
</content>
</invoke>
