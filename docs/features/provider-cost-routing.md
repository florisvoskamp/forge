# Feature: Cost-aware Mesh routing (cheapest capable · subscription-first · quota-aware)

Status: designed (build pending scope confirmation) · Extends ADR-0006 (Model Mesh) and
builds directly on `mesh-routing-finish.md` (PR #27: provider availability + fallback).

This is the request: *"check every model/provider available (only the ones it can actually
use), compare costs, pick the best; optionally prefer an already-paid subscription; and
eventually track usage across subscriptions to choose best vs. the user's subs."*

It turns the mesh from **tier → one configured model** into **tier → choose the cheapest
capable model you can actually use right now**, which is the differentiator's full promise.

## Layers (each shippable independently)

| Layer | What | Status |
|---|---|---|
| **L0 — availability** | only consider models whose provider has a usable key (or needs none) | **done** (PR #27 fallback) |
| **L1 — cheapest capable** | among the usable candidates for a tier, pick the lowest *estimated* cost | this spec, Must |
| **L2 — subscription-first** | treat already-paid subscriptions (CLI bridge) as $0 marginal and prefer them when capable; `prefer_subscription` toggle | this spec, Should (mostly falls out of L1) |
| **L3 — quota-aware** | track per-subscription usage windows; demote a subscription as it nears its limit so the mesh doesn't overrun it | this spec, **Won't (this iteration)** — designed only |

## L1 — cheapest capable (Must)

### JTBD
> When several models could handle a task, I want the mesh to pick the **cheapest one I can
> actually use**, so I get the lowest bill without hand-tuning a single model per tier.

### Config: candidate lists (backward compatible)
Today `mesh.models[tier]` is one model string. Extend each tier to accept **a string OR an
ordered list** (a `OneOrMany`-style enum, same pattern as the permission `allow` field):

```toml
[mesh.models]
trivial  = "ollama::llama3.2"                              # still valid (single)
standard = ["deepseek::deepseek-chat", "gemini::gemini-2.5-flash", "openai::gpt-4o-mini"]
complex  = ["claude-cli::", "anthropic::claude-opus-4-8", "openrouter::deepseek/deepseek-r1"]
```

The list is the **capability set** for that tier (the user asserts every entry can do
tier-level work). Order is the tie-break / preference hint.

### Selection algorithm (deterministic, still zero model calls)
```
1. classify → tier  (unchanged)
2. candidates = mesh.models[tier] as a list
3. usable = candidates.filter(has_usable_key)            # L0
4. if usable empty: cross-tier fallback (PR #27 behaviour), done
5. rank usable by (estimated_cost ASC, config_order ASC); pick the first
6. record rationale: "cheapest of N usable {tier} models: <model> (est $X/1k)"
```

`estimated_cost` = `pricing.cost_for(model, NOMINAL_IN, NOMINAL_OUT)` for a fixed nominal
token mix (e.g. 1000 in / 500 out) — a *relative* comparator, not a forecast. Unpriced
models (gateways, local) are 0.0 → naturally rank cheapest (documented; user can price them
to change ranking). This is the same pricing table FR-5 already maintains.

### Acceptance criteria (L1)
```
AC-L1a  Given standard = ["openai::gpt-4o-mini","deepseek::deepseek-chat"] and BOTH keys set
        When a standard task routes
        Then the cheaper of the two by the pricing table is chosen
        And the rationale names it as cheapest-of-2.

AC-L1b  Given the cheapest candidate's provider has NO key
        When routing
        Then it is skipped and the cheapest *usable* candidate is chosen.

AC-L1c  Given a single-string tier config (legacy)
        When routing
        Then behaviour is identical to today (one candidate).

AC-L1d  Given no candidate in the tier is usable
        Then cross-tier fallback (PR #27) applies.
```

## L2 — subscription-first (Should; mostly emergent)

A subscription you already pay for has **$0 marginal cost** per call. The CLI-bridge models
(`claude-cli::`, `codex-cli::`) are exactly that. Because their `cost_for` is 0.0, L1's
cheapest-first ranking **already prefers them** when they're in a tier's candidate list.

L2 adds only the *explicit control + correctness*:
- `mesh.prefer_subscription` (default `true`): when true, usable subscription/CLI-bridge
  candidates sort ahead of any metered model regardless of the nominal cost compare (so a
  $0 subscription always wins a tie-or-better). When false, treat them purely by cost (still
  $0, so still cheap — the toggle matters once L3 makes "$0 but rate-limited" cost-bearing).
- A model is "subscription" if its provider is a CLI bridge (`provider_of` ∈
  {`claude-cli`,`codex-cli`}). Recorded in the rationale ("preferred your Claude subscription").

### AC (L2)
```
AC-L2a  Given complex = ["claude-cli::","anthropic::claude-opus-4-8"], claude CLI authed,
         anthropic key set, prefer_subscription = true
        When a complex task routes
        Then claude-cli:: is chosen and the rationale says it used the paid subscription.

AC-L2b  Given prefer_subscription = false and the API model is cheaper-by-table than $0…
        (cannot happen while subs are $0 — this toggle only bites under L3 quota costing.)
```

## L3 — quota-aware (Won't build now — designed)

Subscriptions aren't truly unlimited: Claude Pro/Max has rolling usage limits; ChatGPT/Codex
likewise. Treating them as free can overrun the cap and start failing (or, where enabled,
spill to overage). L3 makes the mesh **demote a subscription as it nears its window limit**.

**Data source already in hand:** the CLI-bridge stream surfaces it. Claude Code's stream-json
emits, per turn, a `rate_limit_event`:
```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1781485800,
  "rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false}}
```
and the `result` event carries token usage + `modelUsage`. Codex's `turn.completed` carries
token usage. So Forge can observe remaining headroom without extra calls.

**Design sketch (for when we build it):**
- `CliProvider` parses `rate_limit_event` → returns optional `QuotaHint { kind, resets_at,
  status, fraction_used? }` alongside `ModelResponse` (needs a small `Provider` return
  extension or a side channel).
- `forge-store` persists a `subscription_usage` table keyed by (provider, window kind,
  window start): tokens/requests used, last status, resets_at.
- Router gains a `SubscriptionQuota` input (like `BudgetState`): when a subscription is
  `status != "allowed"` or past a `warn_fraction` of its window, it's demoted below metered
  models in ranking (or skipped if hard-blocked), with a rationale ("Claude 5h limit near —
  routing to <metered>").
- "Best choice vs the user's subs": with both money-cost (metered) and quota-cost
  (subscription headroom) modelled, the router can pick the option that conserves the
  scarcer resource — the full vision in the request.

**Why deferred:** needs a `Provider`/quota side-channel, a new store table + windows, and the
CLI rate-limit schemas are version-volatile (parse defensively, like the bridge parsers).
Meaningful surface; worth its own PR once L1/L2 are in.

**Input now captured (PR: `forge init`).** The static half of L3 — *which plan the user holds*
— is collected by the interactive `forge init` onboarding and stored in
`mesh.subscriptions` (`claude-cli` → e.g. `"max-20x"`, `codex-cli` → `"plus"`). This is the
headroom signal the L3 router will combine with the live `rate_limit_event` usage to demote a
near-limit subscription. Until L3 lands it is informational. Routing today is cost-tiered with
fair provider spread (the `route_score` policy): Trivial→free, Standard→cheap subscription,
Complex→subscription flagship, with within-family ties resolved to the higher-version model
(never a lesser sibling at equal $0 cost).

## Impact (L1 + L2)
| Layer | File | Change |
|---|---|---|
| Config | `forge-config` MeshConfig | `models` value becomes string-or-list (`OneOrMany`); add `prefer_subscription: bool = true`; a `candidates_for(tier) -> Vec<String>` accessor. |
| Pricing | `forge-mesh::pricing` | add `estimated_cost(model)` using a nominal token mix (reuses `cost_for`). |
| Router | `forge-mesh::HeuristicRouter` | step 2–6 above; `is_subscription(model)`; rank usable candidates; rationale. Reuses the injectable availability predicate from PR #27. |
| Subscription detect | `forge-config::provider_of` | already gives the prefix; classify `claude-cli`/`codex-cli` as subscription. |

No change to `Provider`, the turn loop, or the budget gate for L1/L2.

## Definition of done (L1 + L2) — DONE

- [x] `mesh.models` accepts string or list per tier (`OneOrMany`); legacy single-string configs
      unchanged (AC-L1c). New `candidates_for(tier)`.
- [x] Router picks the cheapest *usable* candidate (`cheapest_usable`); ties broken by config
      order; rationale states "cheapest of N usable …" (AC-L1a/b).
- [x] `prefer_subscription` (default true) ranks usable CLI-bridge models first; rationale notes
      "(paid subscription)" (AC-L2a). `pricing::estimated_cost` is the relative comparator.
- [x] No-usable-candidate path falls through to cross-tier fallback (`cross_tier_cheapest`,
      generalised from PR #27 to candidate lists) (AC-L1d).
- [x] Deterministic unit tests via the injectable availability predicate (no ambient-key
      dependence). forge-mesh: 27 tests.
- [x] clippy -D warnings + fmt clean; full workspace green.

**L3 (quota-aware) remains designed-only** — see the sketch above; it needs a `Provider`/quota
side-channel + a `subscription_usage` store table + defensive parsing of the CLI rate-limit
events. Its own PR when wanted.
