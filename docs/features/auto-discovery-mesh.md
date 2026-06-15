# Feature: Auto-discovery Mesh — route to the best *available* model, no config required

> Status: **DESIGN + BUILD** (2026-06-15).

## 1. Problem (JTBD)

> **As** a Forge user who has keys for some providers, **I want** the mesh to **figure out by
> itself** which models I can actually use and send each task to the best one, **so that** I get
> good cost/quality routing **without hand-maintaining a `[mesh.models]` table** that goes stale
> as providers add/remove models.

Today routing is driven by `[mesh.models]` candidate lists (now defaulting to a few free models).
Two problems: (1) the defaults are a hardcoded guess that rots as model ids change; (2) a user
with, say, a Groq key but no OpenAI key still has `openai::gpt-4o-mini` in the shipped standard
tier — a dead candidate. The mesh should instead **discover what's usable** and **rank it**.

### Non-goals
- Replacing the tier **classifier** (trivial/standard/complex) — unchanged.
- Removing config: `[mesh.models]` stays as an **optional override** (full control when set).
- Perfect "best model" truth — model quality is subjective + shifts; we use transparent priors.

## 2. Scope (MoSCoW)

**Must**
- M1 — **Discovery:** on session start, query each provider the user has a key for (and keyless
  local `ollama`) for its model list via `genai::Client::all_model_names`, building a live
  **catalog** of usable `provider::model` ids. Per-provider failures are skipped, not fatal.
- M2 — **Capability ranking:** a built-in, transparent ranker scores each discovered model for a
  tier from **model-family priors** (quality/speed class by id pattern) + **live cost** (free=$0).
  These priors live in Forge code, **not** the user's config (that's what "not hardcoded in
  config" means).
- M3 — **Auto routing:** when a tier has **no explicit config entry**, the router picks the
  top-ranked discovered model for that tier (instead of a shipped default list). Cost-aware +
  subscription-preference rules still apply.
- M4 — **Config override wins:** any tier present in `[mesh.models]` uses the configured
  candidates exactly as today (auto-discovery is the *default*, not a *replacement*).

**Should**
- S1 — Cache discovery for the process (don't re-query providers every turn); refresh on demand.
- S2 — `forge models` command to print the discovered catalog + the per-tier auto pick (so the
  routing is inspectable, ADR-0006 transparency).

**Won't (this turn)**
- Live benchmarking/quality probing of models. Background periodic refresh. Per-model latency
  tracking (the L3 quota-aware idea in provider-cost-routing.md).

## 3. Acceptance criteria

```
Given a GROQ_API_KEY is set and no [mesh.models] config
When the session starts
Then the catalog contains groq:: models discovered from the provider
And a Standard task routes to a capable groq model (top-ranked for the tier)

Given no provider keys at all
When a task is routed
Then it falls back to keyless local ollama (never errors / never an unusable pick)

Given [mesh.models].standard = ["openai::gpt-4o-mini"]
When a Standard task is routed
Then it uses openai::gpt-4o-mini (explicit config overrides auto-discovery)

Given a provider's list endpoint errors or times out
When discovery runs
Then that provider is skipped and the rest of the catalog still builds
```

## 4. Design

### Components
- **`ModelCatalog`** (forge-mesh): `discover(providers_with_keys) -> ModelCatalog`. For each
  usable provider, call `genai::Client::all_model_names(adapter, ProviderConfig::default())`
  (genai resolves the adapter's endpoint + key from env). Map names → `provider::model` ids.
  Always include keyless `ollama` (its own list endpoint). Tolerant: a provider that errors or
  times out contributes nothing. Built **once** at session construction (async), behind a
  bounded timeout per provider, concurrently.
- **`capability` ranker** (forge-mesh): pure fn `tier_score(model_id, tier, cost) -> f64` from
  **family priors** — substring/regex on the id maps to a (quality, speed) class (e.g. `opus`/
  `gpt-4`/`-70b`/`r1` → high quality; `-8b`/`flash`/`mini`/`haiku` → fast/cheap). Per tier:
  Trivial weights speed+cheapness, Complex weights quality, Standard balances. Cost (free=$0)
  breaks ties toward free. Transparent + unit-testable; **the only "hardcoding" is generic
  family priors in code, never specific ids in user config.**
- **Router integration:** `HeuristicRouter` gains an optional `catalog`. In `decide`, for a tier
  with **no config candidates**, build the candidate list from the catalog ranked by
  `tier_score` (top N), then run the existing `cheapest_usable`/subscription logic. Config tiers
  bypass the catalog entirely. Rationale string says "auto-selected from N discovered models".

### Flow
```
session start → providers_with_keys() → ModelCatalog::discover() (concurrent, cached)
route(task): classify tier
  → config has tier? → existing candidate path (override)
  → else → catalog.best_for(tier) via capability ranker → cost-aware pick
```

### Impact
| Layer | Change |
|-------|--------|
| forge-provider | expose a `list_models(provider) -> Vec<String>` helper over `genai::Client::all_model_names` (used by discovery) |
| forge-mesh | `ModelCatalog`, `capability` ranker, `HeuristicRouter` optional catalog + auto path in `decide` |
| forge-config | `provider_of`/`has_api_key`/`known_key_providers` reused to enumerate keyed providers |
| forge-cli | build the catalog at session start (async, before `Session::start`); pass to the router; `forge models` command (S2) |

## 5. Definition of done
- [ ] All §3 acceptance criteria pass (discovery tolerant of failures; config override; ollama floor).
- [ ] `capability` ranker unit-tested per tier (cheap-fast for Trivial, strong for Complex).
- [ ] Discovery is concurrent + per-provider timeout-bounded + cached for the process.
- [ ] `forge models` prints the catalog + per-tier auto pick.
- [ ] fmt + clippy `-D warnings` + full workspace green. Honest note on what can't be tested
      without provider keys (live discovery) — covered by a mock-catalog unit path.
