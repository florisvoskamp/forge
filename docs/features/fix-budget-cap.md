# Fix: real daily/monthly budget cap (FR-5)

> Status: **DRAFT** (2026-06-15, Floris Voskamp). Remediation spec for a confirmed
> requirements regression. Design only — no implementation in this document.

## 1. Problem (JTBD)

**The product's headline differentiator does not actually work.** FR-5 promises a
"daily/monthly cap" that makes the router prefer cheaper tiers and *warn/stop* as the cap
is approached, and §5 (Cost-correctness) states the "budget cap [is] never silently
exceeded." Both claims are currently false.

The bug: the value the core feeds the router as "today's spend" is *not* today's spend. In
`crates/forge-core/src/lib.rs:172-175` the budget is built from
`self.store.session_cost(&self.id)`, and `session_cost`
(`crates/forge-store/src/lib.rs:172-178`) returns `session.total_cost_usd` — the running
total of **one session**. Consequences:

- **No daily aggregation.** Spend is not summed across sessions in a calendar day. Open a
  new session and the "daily" counter resets to `$0`. A user who burns the cap over five
  sessions in an afternoon never trips it.
- **No monthly aggregation at all.** There is no monthly concept anywhere in the schema,
  store, mesh, or config (`daily_budget_usd` is the only cap field —
  `crates/forge-config/src/lib.rs:43`).
- **No hard stop.** `BudgetStatus::Exhausted` only downshifts to the trivial tier and emits
  a `Warning` (`crates/forge-mesh/src/lib.rs:99-103`, `forge-core/src/lib.rs:183-186`).
  Nothing ever *refuses* a call, so a single complex prompt under a downshifted-but-not-zero
  tier can still exceed the cap, and a pinned model ignores the cap entirely.

**Jobs to be done.** As a BYOK solo developer, I want a cap on what Forge can spend *per
calendar day and per calendar month across every session* so that (a) I see live how close
I am, (b) Forge automatically routes cheaper as I approach it, and (c) Forge *stops*
spending my money once I cross it — unless I explicitly override — so the bill is bounded
without me babysitting it.

**Who is affected.** Every user relying on cost control — i.e. the core promise. Currently
the only thing that works is the *per-session* meter; the *budget* is decorative.

## 2. Scope (MoSCoW)

### Must
- **M1.** A `forge-store` aggregation that sums `usage.cost_usd` across **all** sessions for
  a given calendar **day** and calendar **month**, timezone-aware (see §5 timezone decision).
- **M2.** `forge-core` computes today's + this month's spend **before each model call** and
  derives a budget status from the *stricter* of the two caps.
- **M3.** Tiered enforcement: (1) **warn** at `warn_threshold` (default 80%), (2)
  **downshift** the Mesh tier as either cap is approached, (3) **hard stop** — refuse the
  call with a clear message and an override hint — when either cap is exceeded.
- **M4.** Config: `daily_cap_usd`, `monthly_cap_usd`, `warn_threshold`, and behavior
  toggles; all optional (absent cap = unlimited). Backward-compatible with the existing
  `daily_budget_usd`.
- **M5.** Live cost meter in the statusline shows **today vs daily cap** and **month vs
  monthly cap**, not just session spend.
- **M6.** Per-task explicit model pin precedence is defined and enforced (see §5).
- **M7.** Override mechanism to proceed past a hard stop for a single turn / a session.

### Should
- **S1.** A `created_at` index supporting the aggregation query without a table scan.
- **S2.** A `forge cost` / `forge budget` CLI summary (today, month, caps, % used).
- **S3.** Color-graded statusline meter (green / amber / red) tracking budget status.

### Could
- **C1.** Per-provider or per-model sub-caps.
- **C2.** Rolling-window ("last 24h") cap in addition to calendar-day.
- **C3.** Pre-call *estimated* cost gating (reserve estimated spend before streaming).

### Won't (this iteration)
- **W1.** Weekly caps, team/shared budgets, hosted aggregation (out of v0.1 per §3 roadmap).
- **W2.** Hard real-time mid-stream abort once a single call's tokens cross the cap (we
  enforce at call boundaries; see §5 "cost unknown until after streaming").

### Non-goals
- Not changing the pricing model or `Pricing::cost_for` semantics.
- Not introducing a migration framework (single idempotent batch + best-effort `ALTER`
  remains the v0.1 approach, per `schema.rs:1-3`).
- Not making the cap a security boundary against a malicious user — it bounds honest spend.

## 3. Acceptance criteria (Given/When/Then)

Costs below assume the test pricing already used in `forge-core` tests (complex turn ≈
`$0.102` with the pinned `$1.0/1k` override).

### Aggregation across sessions (the core fix)
- **AC-1 (multi-session same-day sum).** *Given* session A recorded `$0.06` today and
  session B recorded `$0.05` today, *when* `spend_today_usd()` is queried, *then* it
  returns `$0.11` — not `$0.05` (B's session total) and not `$0.06`.
- **AC-2 (cap triggers across sessions).** *Given* `daily_cap_usd = $0.10` and session A
  already spent `$0.08` today, *when* a **new** session B starts a turn, *then* B sees
  status `Warning` (0.08 ≥ 0.8·0.10) on its *first* call — proving the counter did **not**
  reset. *When* B's first call pushes the day total to `$0.10+`, *then* B's *next* call is
  hard-stopped.
- **AC-3 (month rollover resets).** *Given* `$5.00` was spent in May and `monthly_cap_usd =
  $4.00`, *when* the system clock is 2026-06-01, *then* `spend_this_month_usd()` returns
  `$0.00` and month status is `Ok` (May spend does not count against June).
- **AC-4 (day rollover resets).** *Given* `$0.20` spent on 2026-06-14 with `daily_cap_usd =
  $0.10`, *when* the clock is 2026-06-15 00:00 local, *then* `spend_today_usd()` returns
  `$0.00` and the day status is `Ok`.

### Tiered enforcement
- **AC-5 (warn band).** *Given* day spend ≥ `warn_threshold·daily_cap`, `< daily_cap`,
  *when* a turn starts, *then* a `Warning` event is emitted naming today's spend and the cap.
- **AC-6 (downshift).** *Given* status `Exhausted` on either cap and a prompt that would
  classify Complex, *when* routing runs, *then* the chosen tier is `Trivial` and the
  rationale mentions the budget.
- **AC-7 (hard stop).** *Given* day spend ≥ `daily_cap` (or month ≥ `monthly_cap`) and
  `behavior.hard_stop = true`, *when* a turn starts, *then* the model call is **refused**
  before any provider request, no `usage` row is written, and the user sees a clear message
  with the override instructions. The turn returns an explanatory string, not an error that
  corrupts session state (§5 Reliability).
- **AC-8 (stricter cap wins).** *Given* day status `Ok` but month status `Exhausted`, *when*
  a turn starts, *then* the effective status is `Exhausted` (hard stop).

### Precedence, override, unlimited
- **AC-9 (pin vs cap).** *Given* a per-task pinned model and an exceeded cap, *when*
  `behavior.cap_overrides_pin = true` (default), *then* the pin is ignored and the call is
  downshifted/stopped per the cap; *when* `false`, *then* the pin is honored and only a
  warning is shown. Behavior is documented and covered by a test for each toggle value.
- **AC-10 (override proceeds once).** *Given* a hard stop, *when* the user supplies the
  override (env `FORGE_BUDGET_OVERRIDE=1`, the `--allow-over-budget` flag, or a TUI `y`
  confirm), *then* exactly the next turn proceeds, a `Warning` records the override, and the
  override does not silently persist beyond its declared scope.
- **AC-11 (no cap = unlimited).** *Given* both caps unset (`None`), *when* any spend
  accrues, *then* status is always `Ok`, no warnings or stops occur, and aggregation
  functions still return correct totals (used only by the meter).

### Edge cases (see §5 table)
- **AC-12 (midnight boundary).** A turn whose calls straddle local midnight attributes each
  `usage` row to the calendar day of *its own* `created_at`; the per-call status check uses
  the day current at check time.
- **AC-13 (concurrent writers).** Two sessions writing `usage` rows in the same day produce
  a correct sum (WAL serializes writes; the aggregate reads committed rows).

## 4. Impact analysis & insertion points

| Crate | File:line (current) | Change |
|-------|--------------------|--------|
| forge-store | `schema.rs:49-58` (`usage` table) | `usage` already has `created_at INTEGER DEFAULT (strftime('%s','now'))`. Add an index `idx_usage_created_at ON usage(created_at)` (S1). No column migration needed. |
| forge-store | `lib.rs:172-178` (`session_cost`) | **Keep** as the per-session meter. Add new fns `spend_today_usd(tz)` and `spend_this_month_usd(tz)` (M1) alongside it. Do **not** repurpose `session_cost`. |
| forge-store | `lib.rs:39-54` (`init`) | Add the new index to `SCHEMA`; idempotent `CREATE INDEX IF NOT EXISTS`. |
| forge-config | `lib.rs:38-55` (`MeshConfig` / `PriceOverride`) | Add `daily_cap_usd`, `monthly_cap_usd: Option<f64>`, `warn_threshold: f64` (default 0.8), `budget: BudgetBehavior` struct (M4). Alias/migrate the existing `daily_budget_usd`. |
| forge-config | `lib.rs:57-78` (`Default`) | Default new fields (caps `None`, `warn_threshold 0.8`, behavior defaults). |
| forge-mesh | `lib.rs:11-41` (`BudgetState` / `status`) | Extend `BudgetState` with month fields + `warn_fraction`; status considers the stricter of day/month (M2, AC-8). |
| forge-mesh | `lib.rs:93-113` (`HeuristicRouter::route`) | Downshift hook already keyed on `status()`; extend to the combined status. Add `cap_overrides_pin` handling at the pin site (M6). |
| forge-core | `lib.rs:170-188` (`run_turn` budget build + status match) | Replace `session_cost` with the new aggregation; add the **pre-call hard-stop gate** here, before the step loop (M2, M3, AC-7). |
| forge-core | `lib.rs:228-258` (post-call cost record) | Unchanged recording path; the new gate reads what this writes. |
| forge-tui | `app.rs:59-79` (`App`) + `app.rs:341-398` (`render_statusline`) | Add `today_usd`, `daily_cap`, `month_usd`, `monthly_cap`, `budget_status` to `App`; render meter (M5, S3). |
| forge-tui | `lib.rs:18-50` (`PresenterEvent`) | Add a `Budget { today_usd, daily_cap, month_usd, monthly_cap, status }` event (or extend `Cost`); emit each turn. |
| forge-cli | (cost wiring) | Wire the new event; add `forge cost`/`forge budget` subcommand (S2) and `--allow-over-budget` flag (M7). |
| workspace | `Cargo.toml` | Add `chrono` (MIT/Apache-2.0, satisfies §6 licensing) for timezone-aware day/month boundaries — no time crate currently present. |

**Blast radius:** additive. `session_cost` and the per-session meter keep working; existing
tests (`cost_accumulates_for_a_priced_model`, `warns_when_budget_threshold_reached`) stay
green if the day aggregation equals the single in-memory session's spend (it does for a
one-session test). New behavior is gated behind new config fields that default to today's
behavior when caps are unset.

## 5. Technical design

### 5.1 Timezone decision: **local time**, computed in Rust

Caps are framed as "daily/monthly" — a human, calendar concept. A user in UTC+13 expects
the day to roll at *their* midnight, not 13 hours early. So boundaries are **local**.

But SQLite's `strftime('%s','now')` stores **UTC** epoch seconds (correct, unambiguous), and
`strftime('%Y-%m-%d', created_at)` without a modifier groups by **UTC** day — wrong for us.
Rather than embed a fixed `'+HH:MM'` (which breaks across DST), compute the day/month
**window bounds in Rust** with `chrono::Local` and pass epoch-second bounds as parameters:

```rust
// forge-store: timezone-aware window helpers (local calendar).
use chrono::{Local, TimeZone, Datelike, Duration};

fn day_bounds_local(now: chrono::DateTime<Local>) -> (i64, i64) {
    let start = now.date_naive().and_hms_opt(0,0,0).unwrap();
    let start = Local.from_local_datetime(&start).earliest().unwrap();
    (start.timestamp(), (start + Duration::days(1)).timestamp())
}

fn month_bounds_local(now: chrono::DateTime<Local>) -> (i64, i64) {
    let first = now.date_naive().with_day(1).unwrap().and_hms_opt(0,0,0).unwrap();
    let start = Local.from_local_datetime(&first).earliest().unwrap();
    let next  = if first.month() == 12 { first.with_year(first.year()+1).unwrap().with_month(1) }
                else { first.with_month(first.month()+1) }.unwrap();
    let end = Local.from_local_datetime(&next).earliest().unwrap();
    (start.timestamp(), end.timestamp())
}
```

`earliest()` resolves the DST spring-forward gap (a nonexistent local midnight maps to the
next valid instant); `latest()` is unnecessary because we only need a half-open `[start,end)`
window. This is robust to DST and to the user changing the system clock between calls (each
call recomputes bounds from the current `Local::now()`).

### 5.2 Store aggregation (M1) — SQL + signature

```rust
// crates/forge-store/src/lib.rs  (new, beside session_cost at :172)

/// Total spend across ALL sessions whose usage rows fall in [start, end) epoch seconds.
fn spend_between(&self, start: i64, end: i64) -> Result<f64> {
    Ok(self.lock()?.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage \
         WHERE created_at >= ?1 AND created_at < ?2",
        (start, end),
        |row| row.get(0),
    )?)
}

pub fn spend_today_usd(&self) -> Result<f64> {
    let (s, e) = day_bounds_local(Local::now());
    self.spend_between(s, e)
}

pub fn spend_this_month_usd(&self) -> Result<f64> {
    let (s, e) = month_bounds_local(Local::now());
    self.spend_between(s, e)
}
```

Schema addition (idempotent, append to `SCHEMA` in `schema.rs`):

```sql
CREATE INDEX IF NOT EXISTS idx_usage_created_at ON usage(created_at);
```

We aggregate over `usage.cost_usd` (the authoritative per-call cost) rather than
`session.total_cost_usd`, because (a) `usage` carries `created_at` per call so a session
straddling midnight is split correctly (AC-12), and (b) it is the same source of truth the
per-session total is derived from. For tests, allow injecting `now` and the timezone via an
internal `spend_between` + test-only constructors so AC-3/AC-4 can pin a clock without
waiting for real midnight.

### 5.3 Mesh budget state (M2, AC-8)

```rust
// crates/forge-mesh/src/lib.rs  (extend BudgetState at :11)
pub struct BudgetState {
    pub spent_today_usd: f64,
    pub daily_cap_usd: Option<f64>,
    pub spent_month_usd: f64,
    pub monthly_cap_usd: Option<f64>,
    pub warn_fraction: f64, // from config.warn_threshold
}

impl BudgetState {
    fn axis(spent: f64, cap: Option<f64>, warn: f64) -> BudgetStatus {
        match cap {
            Some(c) if spent >= c          => BudgetStatus::Exhausted,
            Some(c) if spent >= c * warn   => BudgetStatus::Warning,
            _                              => BudgetStatus::Ok,
        }
    }
    /// Stricter of the two axes wins (Exhausted > Warning > Ok).
    pub fn status(&self) -> BudgetStatus {
        Self::axis(self.spent_today_usd, self.daily_cap_usd, self.warn_fraction)
            .max(Self::axis(self.spent_month_usd, self.monthly_cap_usd, self.warn_fraction))
    }
}
```

`BudgetStatus` gains `#[derive(PartialOrd, Ord)]` with the order `Ok < Warning < Exhausted`
so `.max()` expresses "stricter wins" (AC-8). The existing `WARN_FRACTION = 0.8` becomes the
default for `warn_fraction`. **Back-compat:** keep a `daily_budget_usd` accessor that maps to
`daily_cap_usd` so existing config and tests still parse.

### 5.4 Core enforcement gate (M3, AC-7) — placement & precedence

Insert a gate in `run_turn` **between** building `budget` (currently
`forge-core/src/lib.rs:172`) and the step loop (`:207`). Order of operations:

1. Compute `spent_today = store.spend_today_usd()`, `spent_month = store.spend_this_month_usd()`.
2. Build `BudgetState` from those + config caps + `warn_threshold`.
3. **Resolve precedence** (M6):
   - If the task has an explicit pin **and** `behavior.cap_overrides_pin == false`: honor the
     pin; on `Exhausted` emit a `Warning` only (no downshift, no stop).
   - Otherwise the cap governs: pass `budget` into `router.route` (downshift, AC-6).
4. **Hard stop** (`status == Exhausted` && `behavior.hard_stop` && no active override):
   emit a `Warning` describing today/month spend vs caps and how to override, **return the
   explanatory text without making any provider call** (no `usage` row written, transcript
   gets the user message + a system note; AC-7). The override (env/flag/TUI confirm, M7)
   clears the gate for exactly this turn (AC-10).
5. **Warn** (`status == Warning`): emit `Warning`, continue (AC-5).
6. **Downshift** is handled inside `router.route` via `budget.status()` (already wired at
   `mesh/lib.rs:99`), now driven by the combined status.

Precedence summary (highest first): **override (one turn)** → **hard stop** → **cap downshift**
→ **explicit pin** → **heuristic classification**. With `cap_overrides_pin = false`, the pin
moves above "cap downshift" but never above "hard stop" unless the user also overrides.

Because per-call cost is unknown until after streaming (§5.6), the gate is **pre-turn** and
checks *accumulated* spend; the first call that *crosses* the cap is allowed to finish (it
was Ok at check time), and the *next* call is stopped (AC-2). This bounds overrun to at most
one in-flight call — acceptable for v0.1 and called out as W2.

### 5.5 Config (M4)

```toml
[mesh.budget]
daily_cap_usd     = 5.00     # optional; absent = unlimited
monthly_cap_usd   = 80.00    # optional; absent = unlimited
warn_threshold    = 0.8      # fraction of a cap that triggers a warning
hard_stop         = true     # refuse calls once a cap is exceeded
cap_overrides_pin = true     # a cap downshifts/stops even a pinned model
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetBehavior {
    pub hard_stop: bool,         // default true
    pub cap_overrides_pin: bool, // default true
}
```
`MeshConfig` gains `daily_cap_usd`, `monthly_cap_usd: Option<f64>`, `warn_threshold: f64`
(default 0.8), `budget: BudgetBehavior`. `daily_budget_usd` is retained as a deprecated alias
mapped into `daily_cap_usd` at load time so existing configs/tests keep working.

### 5.6 Cost unknown until after streaming (estimate vs actual)

Cost is computed *after* a call returns (`forge-core/src/lib.rs:229-233`). The gate therefore
uses **actual accumulated** spend, not a pre-call estimate. This is deliberate for v0.1:
estimation requires guessing output length and is itself error-prone. Trade-off: a single
call can push spend past the cap before the next gate catches it (≤ one call of overrun,
bounded by the per-call max cost). C3 (reserve an estimate before streaming) is a future
tightening; W2 documents that we do not abort mid-stream.

### 5.7 Statusline meter (M5, S3) — mockup

The meter replaces the bare `$x.xxxx` session figure with day/month-vs-cap, color-graded by
`budget_status`. Session spend stays available but the *budget* is primary.

```
 ⠹ working  ·  [complex] anthropic::claude-opus-4-8  ·  today $3.80/$5.00  mo $61/$80   ↵ send · esc quit
                                                          └ amber (Warning)  └ green (Ok)
```
Exhausted (hard stop pending override):
```
 [trivial] ollama::llama3.2  ·  today $5.02/$5.00 ⛔  mo $61/$80    over budget · esc quit
                                  └ red
```
No cap configured (unlimited) falls back to the current behavior — show running session/day
spend with no `/cap` denominator:
```
 [standard] openai::gpt-4o-mini  ·  today $0.0042                  ↵ send · esc quit
```
Narrow-terminal degradation follows the existing priority order in `render_statusline`
(`app.rs:362-396`): drop `mo …`, then the tier tag, then the spinner; the `today $x/$cap`
segment is the last to drop.

### 5.8 Edge-case table

| Edge case | Handling |
|-----------|----------|
| Midnight boundary mid-turn | Each `usage` row keyed by its own `created_at`; gate recomputes day bounds from `Local::now()` at each turn (AC-12). |
| Month boundary | `month_bounds_local` returns `[first-of-month, first-of-next-month)`; year wrap handled (AC-3). |
| DST spring-forward (no local midnight) | `from_local_datetime(...).earliest()` maps the gap to the next valid instant; window stays half-open and contiguous. |
| DST fall-back (ambiguous midnight) | `earliest()` picks the first occurrence; at most a 1-hour skew in the boundary, never a gap or double-count. |
| User changes system clock backward | Bounds recomputed each call; spend already recorded under old timestamps still sums correctly by `created_at`. A large backward jump could re-open a "today" — accepted; honest-user model, documented. |
| Concurrent sessions writing cost | WAL serializes writes; `SUM` reads committed rows. Reads may lag an in-flight uncommitted write by one call — same bound as W2 (AC-13). |
| No cap configured | `Option<f64> = None` ⇒ axis is always `Ok`; aggregation still runs for the meter (AC-11). |
| Cost unknown until after stream | Gate uses accumulated actuals; ≤ one call overrun (§5.6, W2). |
| Override scope leak | Override is consumed per turn (env read once / flag for the invocation / TUI confirm per stop); a `Warning` records that an over-budget call was permitted (AC-10). |
| Local/Ollama (free) calls | `cost_usd = 0` rows still recorded; they never advance spend, so a downshifted-to-Ollama session can continue indefinitely under a hard cap (intended). |

## 6. Definition of done

- `spend_today_usd` / `spend_this_month_usd` exist in `forge-store`, sum `usage.cost_usd`
  across all sessions over local calendar windows, and are backed by `idx_usage_created_at`.
- `session_cost` is unchanged and still powers the per-session figure.
- `BudgetState` carries day + month spend/caps; `status()` returns the stricter axis.
- `run_turn` gates **before** the step loop: warn → downshift (via router) → hard stop, with
  documented precedence vs explicit pins and a per-turn override.
- Config exposes `daily_cap_usd`, `monthly_cap_usd`, `warn_threshold`, and
  `budget.{hard_stop, cap_overrides_pin}`; `daily_budget_usd` still parses (deprecated alias).
- Statusline shows today/month spend vs caps, color-graded, with the documented
  narrow-terminal degradation; unlimited mode shows no denominator.
- `chrono` added to the workspace; licensing compatible (§6 requirements).
- Tests cover **AC-1 … AC-13**, explicitly including: two sessions summing within a day
  (AC-1), a cap tripping across a session boundary (AC-2), and month/day rollover resets
  (AC-3, AC-4) with an injectable clock. Existing budget/cost tests remain green.
- `docs/architecture/01-requirements.md` FR-5 and §5 Cost-correctness are re-verified against
  the new behavior (the cap is now genuinely "never silently exceeded" modulo the documented
  ≤ one-call overrun, W2).
```
