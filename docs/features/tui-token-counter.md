# Feature: live token counter + context-window gauge (statusline)

> Small TUI feature in `crates/forge-tui`. Adds live token usage next to the spinner/cost
> in the statusline, plus a context-window fill gauge. Pairs with the existing cost meter.

## 1. Problem (JTBD)

> When I'm in a turn, I want to see how many tokens I'm spending **and how full the model's
> context window is**, so I know when I'm burning budget or about to overflow context —
> without doing mental math from the cost figure alone.

The statusline already shows `$cost` (`render_statusline`, `crates/forge-tui/src/app.rs`),
but cost is an opaque proxy — it doesn't tell you token volume or, more importantly, how
close you are to the context-window limit (the thing that forces a compaction/truncation).
Claude Code shows a context meter; Forge should too, and better — alongside the live Model
Mesh state it already renders.

Who's affected: every interactive user. Why it matters: token volume + context fill are the
two numbers that predict the next problem (cost spike, context overflow); cost alone hides both.

## 2. Scope (MoSCoW)

**Must have**
- A token segment in the statusline showing **session total** input/output tokens, updated
  live as turns complete (next to the spinner + cost).
- A **context-window gauge**: tokens currently in the live context vs the active model's
  limit, shown as `used/limit` and a percentage (e.g. `18.2k/200k · 9%`).
- Graceful width degradation: the gauge/token segment drop on narrow terminals before
  model+cost (which stay highest priority), consistent with the existing priority ladder.

**Should have**
- Color thresholds on the context gauge: dim < 70%, yellow ≥ 70%, red ≥ 90% (warn before
  overflow). Reuse the existing palette (`DIM`/`WARNYEL`/`ERRRED`).
- Compact humanized formatting (`1.2k`, `34.5k`, `1.1M`).

**Could have**
- Per-turn token delta (tokens the last turn added) shown transiently.
- A tiny inline bar glyph for the gauge (e.g. `▰▰▰▱▱`).

**Won't have (this iteration)**
- Historical token charts; per-tool token breakdown; changing how tokens are counted.

## Non-goals
- Does **not** change cost calculation or the agent loop. It only *displays* existing
  `Usage` data plus a per-model context limit.
- Does **not** implement context compaction (separate roadmap item) — it only surfaces the
  fill level that would *trigger* it.

## 3. Acceptance criteria

```
Given a session with completed turns
When the TUI renders the statusline
Then it shows session input+output tokens (humanized) next to the cost segment

Given the active model has a known context-window limit
When the TUI renders
Then it shows a context gauge "used/limit · N%" reflecting tokens in the current context
And the percentage color is dim <70%, yellow ≥70%, red ≥90%

Given a turn completes and adds tokens
When the next render occurs
Then the token total and context gauge update to the new values (live)

Given a terminal too narrow for all segments
When the TUI renders
Then the gauge drops first, then the token segment, before model+cost are touched

Given the active model's context limit is unknown
When the TUI renders
Then the gauge shows tokens used with no "/limit · %" (no fabricated denominator)
```

## 4. Impact analysis

Single crate for rendering; one small data plumb from core. No agent-loop logic change.

| Layer | Insertion point | Change |
|-------|-----------------|--------|
| Types | `forge-types` `Usage` (`lib.rs:104`, fields `input_tokens`/`output_tokens`/`cost_usd`, `total_tokens()`) | none — reuse |
| Event | `forge-tui::PresenterEvent::Cost` (`lib.rs:44`) | extend to carry token totals + context tokens + context limit (rename to `Usage`/`Meter`, or add fields): `{ session_total_usd, session_in, session_out, context_tokens, context_limit: Option<u32> }` |
| Core emit | `forge-core` emits `Cost` after each turn (`lib.rs:282`) | populate the new fields from accumulated `Usage` (session in/out from store) + current context size; context size = sum of message tokens in the live transcript |
| Context limit | `forge-mesh`/pricing (already knows models for `cost_for`, `lib.rs:229`) | add a `context_limit(model) -> Option<u32>` lookup (per-model window map; `None` if unknown) |
| Store | `forge-store` | add `session_tokens(id) -> (u64,u64)` aggregation (sums alongside the existing `session_cost`) |
| Render | `forge-tui::app::render_statusline` (`app.rs`) | add token + gauge segments with the existing width-priority pattern; `App` gains `session_in`/`session_out`/`context_tokens`/`context_limit` fields fed by `apply` |

## 5. Technical design

### Statusline layout (extends the current segments)

Current: ` ⠙ working  ·  [tier] model  ·  $0.0033            ↵ send · esc quit`

New (widest form):
```
 ⠙ working · [complex] ollama::llama3.2 · ↑12.3k ↓4.1k · ◷ 18.2k/200k 9% · $0.0033   ↵ send · esc quit
```
- `↑in ↓out` = session input/output tokens (humanized).
- `◷ used/limit N%` = context-window gauge; `N%` colored by threshold.
- `$cost` unchanged.

Width priority (drop order on shrink, lowest first): hints → context gauge → token segment
→ tier → (model + cost always shown). Implemented with the same `w >= N` ladder already in
`render_statusline`.

Narrow fallback (no known limit): `↑12.3k ↓4.1k` and `◷ 18.2k` (used only).

### Data flow
`run_turn` already records `Usage` per message and emits `Cost` (`lib.rs:258,282`). Extend
that emit: pull `session_tokens(id)` for the totals, compute `context_tokens` as the token
sum of the messages currently in the live context, and look up `context_limit(model)` from
the mesh's per-model table. `App::apply` stores them; `render_statusline` formats them. No
new event timing — it rides the existing post-turn `Cost` emit, so it updates exactly when
cost does.

### Humanize helper
`fn human(n: u64) -> String` → `< 1000` as-is; `< 1_000_000` → `{:.1}k`; else `{:.1}M`.
Pure, unit-tested.

### Edge cases
| Edge case | Behaviour |
|-----------|-----------|
| Context limit unknown for model | show used tokens only, omit `/limit · %` (no fake denominator) |
| Very narrow terminal | gauge → token segment drop before model/cost (priority ladder) |
| Context exceeds limit (overflow) | gauge shows ≥100% in red (signals compaction needed) |
| Tokens not yet known (before first turn) | `↑0 ↓0`, gauge hidden until a limit + usage exist |
| Streaming mid-turn | totals update on turn completion (existing `Cost` cadence); no per-token statusline thrash |
| Huge counts (M+ tokens) | humanized to `M`, never overflows the segment |

## 6. Definition of done
- [ ] Statusline shows session ↑in/↓out tokens next to cost.
- [ ] Context gauge shows `used/limit · %` with threshold colors; omits `/limit·%` when limit unknown.
- [ ] Width-priority drop order verified at 3 widths (TestBackend): gauge → tokens → tier → (model+cost stay).
- [ ] `human()` and `context_limit()` unit-tested; per-model limit table covers the configured providers.
- [ ] Token/gauge values update live on turn completion (driven by the existing `Cost`/`Usage` emit).
- [ ] `cargo fmt` + `clippy -D warnings` clean; existing statusline tests still pass.
- [ ] No change to cost math or the agent loop.
