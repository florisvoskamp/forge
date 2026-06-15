# Feature: Finish the Model Mesh (pin · fallback · richer signals)

Status: designed · Implements the open **follow-ups of ADR-0006** (Model Mesh) + roadmap
FR-4 caveats / "Model selection UX" (P2). The mesh already routes deterministically with a
real budget cap (PR #19) and records a rationale; this closes the three gaps an honest audit
found.

## Problem (JTBD)

> When the mesh mis-tiers a task, or the routed provider has no key, I want to **override the
> model** and have the mesh **fall back to a model I can actually use** — and I want the tier
> heuristic to read more than prompt length — so routing is trustworthy, not a coin flip.

ADR-0006 explicitly listed these as follow-ups: a richer signal set, user override/pin (its
own mitigation for mis-tiering), and the budget→tier policy (already done). Provider fallback
became important once PR #25 added six providers: a tier pointing at a provider with no key
currently hard-errors instead of degrading.

## Scope (MoSCoW)

**Must have**
1. **Model pin / override** — `forge run --model <id>` / `forge chat --model <id>` forces that
   model, bypassing classification. Rationale records "pinned (--model)".
2. **Provider fallback** — if the routed model's provider has no usable key, route to the most
   capable configured tier model whose provider *does* (or that needs none, e.g. `ollama::`,
   `claude-cli::`). Rationale records the fallback. If none is usable, keep the original (it
   errors downstream with the existing actionable MissingKey message).
3. **Richer classification signals** — beyond length + complex-keyword: explicit user hints,
   code-vs-prose, and dev-action verbs. Deterministic, zero extra cost (ADR-0006 / A-2).

**Should have**
- Pin honours the budget contract: a hard cap still blocks (already enforced pre-routing);
  with `cap_overrides_pin = true` (default) an *exhausted* budget downshifts even a pinned
  model.

**Won't have (this iteration)**
- In-session `/model` command (needs the command system — separate feature).
- Cost-comparison among candidate models (beyond ADR-0006 scope).
- A cheap-LLM classifier (ADR-0006 keeps it pluggable; not now).
- Config-file pin (`[mesh] pin`) — CLI flag only for v1.

## Acceptance criteria

```
AC-1  Given `forge run --model openai::gpt-4o "x"` and the budget is OK
      When the task is routed
      Then the chosen model is `openai::gpt-4o`
      And the rationale says it was pinned.

AC-2  Given a pinned model AND the budget is Exhausted AND cap_overrides_pin = true
      And hard_stop = false (so the pre-routing block doesn't fire)
      When routed
      Then the pin is ignored and the trivial-tier model is chosen (budget wins).

AC-3  Given mesh.models.complex = "anthropic::…" and NO anthropic key,
       but mesh.models.trivial = "ollama::llama3.2" (needs no key)
      When a complex task is routed
      Then the mesh falls back to an available model (not anthropic)
      And the rationale notes the fallback.

AC-4  Given no configured tier model has a usable key
      When routed
      Then the originally-chosen model is returned unchanged (degrades to today's behaviour:
           the provider call surfaces the actionable MissingKey error).

AC-5  Given a prompt containing a fenced code block or dev verbs ("debug", "refactor"…)
      When classified
      Then the tier is at least Standard (not Trivial), regardless of length.

AC-6  Given a short prompt with an explicit "think hard"/"carefully" hint
      When classified
      Then the tier is Complex.

AC-7  Given a short plain prompt with no signals (e.g. "fix typo")
      When classified
      Then the tier is Trivial (existing behaviour preserved — no regressions).
```

## Technical design

### Insertion points
- `crates/forge-config/src/lib.rs` — add `provider_of(model) -> &str` (prefix before first
  `::`) and `has_api_key(provider) -> bool` (true for keyless providers; else env-or-keyring
  present). Non-erroring; used by the router.
- `crates/forge-mesh/src/lib.rs` — `HeuristicRouter` gains `pin: Option<String>` + `with_pin`;
  `classify` gains the new signals; `route` applies pin → budget → fallback in that order.
- `crates/forge-cli/src/main.rs` — `--model` on `Run`/`Chat`; thread `pin: Option<String>`
  through `build_session(_with)` → `HeuristicRouter::new(config).with_pin(pin)`.

### route() order of operations
```
1. base = if pin set AND NOT (exhausted && cap_overrides_pin):
              { tier = classify(prompt).0 (for stats), model = pin, rationale = "pinned (--model)" }
          else:
              normal classify → tier → model_for(tier), with exhausted→trivial downshift (today)
2. final model = fallback(base.model):   # provider-key aware
       if has_key(provider_of(model)) -> model
       else first of [complex, standard, trivial] model_for whose provider has_key
            (append "— fell back to <m> (no key for <model>)" to rationale)
       else model unchanged
```
Pin can't bypass a hard cap: `run_turn` already blocks before routing when
`Exhausted && hard_stop && !override`. The `cap_overrides_pin` branch only matters when
`hard_stop = false`.

### classify() signal set (deterministic, ordered)
1. explicit **complex hint** (`think hard`, `think deeply`, `ultrathink`, `carefully`,
   `step by step`) → Complex.
2. complex **keyword** (existing list) OR length > 600 OR a **fenced code block** with
   substance → Complex.
3. **code present** (fences, or high code-symbol density) OR **dev verb**
   (`debug`, `refactor`, `optimize`, `implement`, `migrate`, `benchmark`, `profile`) OR
   mid length → Standard.
4. explicit **trivial hint** (`quick`, `simple`, `one-liner`) AND short → Trivial.
5. length < 80 and no signals → Trivial.
6. else → Standard.

Order preserves existing tests: "fix typo" (no signal, <80) → Trivial; "refactor …" → Complex
(keyword); medium endpoint prompt → Standard.

## Definition of done
- [ ] AC-1…AC-7 covered by unit tests in `forge-mesh` (+ config tests for `has_api_key`/`provider_of`).
- [ ] `--model` flag works on `run` and `chat`; rationale shows pin/fallback in the `Routing` event.
- [ ] Existing mesh/core tests stay green (no regressions).
- [ ] clippy `-D warnings` + fmt clean; CI green on all platforms.
- [ ] ADR-0006 follow-up items (signal set, pin) marked addressed; roadmap FR-4 caveat updated.
