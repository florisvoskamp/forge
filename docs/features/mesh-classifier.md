# Feature: Better task classifier + optional cheap-LLM classifier

Status: designed · Extends ADR-0006 (Model Mesh). Closes two of its remaining follow-ups:
a richer signal set (today's classifier is still mostly length+keyword) and the opt-in
cheap-LLM classifier (ADR-0006 option 2, deferred because A-2 forbids per-task model calls
*by default* — this makes it explicitly opt-in).

## Problem (JTBD)

> When the mesh decides a task's difficulty, I want it to judge the **actual cognitive load
> of the request**, not its character count, so a hard short prompt isn't billed to a weak
> model and a long-but-trivial one isn't billed to an expensive one.

Observed pain (real run): the classifier mis-tiers on length. A 74-char real standard
request dropped to **trivial** purely because `< 80`. "design a lock-free queue" (24 chars)
would classify **trivial** — exactly backwards. The current `classify()` is a first-match
ladder (hint → keyword/length → trivial-hint → code/verb → length), so length still
dominates the common path.

**Is this the right feature?** Yes — routing quality *is* the mesh, and the user explicitly
hit the failure. The LLM classifier is the long-promised adaptive option for users who'd
trade a tiny cheap call for better routing (free if pointed at a subscription bridge).

## Scope (MoSCoW)

**Must have (Part A — precise heuristic)**
- A transparent **weighted-scoring** classifier: every signal adds/subtracts points with a
  reason; the total maps to a tier. No single signal (esp. length) dominates. Zero added
  cost/latency, deterministic (ADR-0006 / A-2).
- Capability signals that override length: reasoning verbs, algorithmic/proof terms,
  multi-step scope, code, error/stack traces — so a hard *short* prompt → Complex.
- The `RoutingDecision.rationale` lists the firing signals (transparency).

**Should have (Part B — cheap-LLM classifier, opt-in)**
- `LlmRouter`: asks a small cheap model for the tier before routing; falls back to the
  heuristic on any error/timeout/unparseable reply. Off by default.
- Config to select it and pick the classifier model (a $0 subscription bridge works).

**Could have**
- Cache identical-prompt classifications within a session.
- Per-signal weight overrides in config.

**Won't have (this iteration)**
- Learning/feedback loop that tunes weights from outcomes.
- Embeddings / similarity routing.
- Streaming the classifier model's reasoning.

### Non-goals
- This feature does **not** make the LLM classifier the default (A-2 stands: no per-task
  model call unless the user opts in).
- It does **not** change tier→model selection (that's cost-aware routing, already shipped) —
  only how the *tier* is decided.

## Acceptance criteria

### Part A — heuristic (regressions + the key improvement)
```
AC-A1  Given "fix typo"                          → Trivial   (negative/short signals win)
AC-A2  Given "refactor the auth module"          → Complex   (reasoning verb)
AC-A3  Given the 146-char email-validation+wire prompt → Standard (action + multi-step, no deep reasoning)
AC-A4  Given a HARD SHORT prompt "design a lock-free queue" (24 chars) → Complex
        (capability over length — the headline fix; length alone would say Trivial)
AC-A5  Given "prove this sort is stable"         → Complex   (proof/analysis term, short)
AC-A6  Given "rename foo to bar in utils.rs"     → Trivial   (rename + single file, even with a path)
AC-A7  Given a 700-word prose dump with no code/verbs → Standard or Complex by score,
        not auto-Complex purely for length (length is one weighted signal, capped)
AC-A8  Every decision's rationale names ≥1 concrete signal that fired (not just "length").
```

### Part B — LLM classifier
```
AC-B1  Given mesh.classifier="llm" and a reachable classifier model
       When a turn is routed
       Then exactly ONE cheap classification call is made, its one-word answer is parsed to a
            tier, and routing uses that tier; rationale notes "classified by <model>".

AC-B2  Given the classifier model errors / times out / returns gibberish ("banana")
       When routing
       Then the router silently falls back to the heuristic tier (no turn failure)
       And the rationale notes the fallback.

AC-B3  Given mesh.classifier="heuristic" (default / unset)
       Then NO classification model call is ever made (A-2 preserved).

AC-B4  Given classifier="llm" but no classifier_model configured
       Then it falls back to the trivial-tier model id, or to heuristic if none — never errors.
```

## Impact analysis

| Layer | File:line | Change |
|---|---|---|
| Router trait | `forge-mesh/src/lib.rs` `Router::route` | **becomes `async fn`** (so an impl can do I/O). `#[async_trait]`. |
| Heuristic | `forge-mesh/src/lib.rs` `HeuristicRouter::classify` | replace the ladder with `score(prompt) -> Classification { tier, score, reasons }`; `route` becomes `async` (trivially). |
| LLM router | `forge-mesh/src/llm_router.rs` (new) | `LlmRouter { provider: Arc<dyn Provider>, model: String, fallback: HeuristicRouter }`; one cheap call, parse, fallback. |
| Core | `forge-core/src/lib.rs` run_turn | `self.router.route(...)` → `.await` (already async fn). |
| Config | `forge-config` MeshConfig | add `classifier: ClassifierKind` (default Heuristic) + `classifier_model: Option<String>`. |
| CLI wiring | `forge-cli` build_session_with | build `LlmRouter` (sharing the session's provider) when `classifier="llm"`, else `HeuristicRouter`. |
| Provider | `forge-provider` | none — `Provider` is already `async + Send + Sync`; `LlmRouter` reuses it. |

**Async ripple:** `Router` is small. Callers: forge-core (one call, in async fn → add `.await`),
forge-mesh tests (add `.await`, make `#[tokio::test]`), forge-cli (constructs, doesn't call).
`Router: Send` already; with `#[async_trait]` add `Send` bound on the future. `HeuristicRouter`
holds an injected availability fn — still `Send`.

**Regression risk:** the scoring rewrite touches the hottest mesh path. Mitigation: keep all
existing classification tests (AC-A1–A3) + add the new ones (A4–A8); the cost-aware/pin/
fallback tests are downstream of tier and must still pass after the async change.

## Technical design

### Part A — weighted scoring (deterministic)

```
struct Classification { tier: TaskTier, score: i32, reasons: Vec<&'static str> }

fn score(prompt) -> Classification:
    points = 0; reasons = []
    tokens = word_count(prompt)               // estimate, not raw chars

    // length as ONE capped signal (not the decider)
    points += clamp(tokens-based, -2 ..= +3)   // long nudges up; tiny nudges down

    // capability signals (can carry a short prompt to Complex)
    +4  reasoning/algorithmic terms: design, architect, debug, optimi, concurren,
        lock-free, race, prove, complexity, invariant, distributed, why, explain, analyze
    +3  fenced ``` code block, or high code-symbol density
    +2  action verbs: implement, migrate, integrate, refactor, benchmark, profile
    +2  multi-step scope: "and then", " then ", enumerated "1." / "2.", ≥2 file paths
    +1  mentions tests / benchmark / edge cases
    +1  error/stack-trace text present (panic, traceback, "error[", " at line ")

    // user hints (strong, explicit)
    +5  COMPLEX_HINTS (think hard, carefully, step by step, ultrathink)
    -5  TRIVIAL_HINTS (quick, simple, one-liner)

    // negative/trivial patterns
    -4  typo, rename, bump version, format, lint, "add a comment", "fix import"
    -1  single short imperative, no other signal

    tier = Trivial  if score <= TRIVIAL_MAX (e.g. 1)
           Complex  if score >= COMPLEX_MIN (e.g. 6)
           Standard otherwise
    reasons = the signals that fired, highest weight first
```

Worked checks (illustrative, exact weights tuned in code to satisfy ACs):
| prompt | dominant signals | tier |
|---|---|---|
| `fix typo` | trivial pattern (-4), tiny (-1) | **Trivial** |
| `design a lock-free queue` | reasoning "design"+"lock-free" (+4,+4) | **Complex** |
| `prove this sort is stable` | "prove" (+4) | **Complex** |
| `rename foo to bar in utils.rs` | trivial "rename" (-4) beats 1 path (+0) | **Trivial** |
| email-validate + wire handler (146c) | action verb (+2), multi-step (+2), length (+1) | **Standard** |
| `refactor the auth module` | "refactor" reasoning (+4) | **Complex** |

The thresholds + weights are chosen so the AC table holds; tests pin them.

### Part B — LLM classifier

```
#[async_trait] impl Router for LlmRouter {
  async fn route(&self, prompt, budget) -> RoutingDecision {
     // 1 cheap call, hard timeout
     let sys = "You classify a software task's difficulty. Reply with ONE word: \
                trivial, standard, or complex. No punctuation.";
     match timeout(T, self.provider.complete(&self.model, [system(sys), user(prompt)], &[], &mut sink)).await {
        Ok(Ok(resp)) => match parse_tier(resp.content) {       // first word, lowercased
           Some(tier) => decision(tier, "classified by {model}"),
           None       => self.fallback.route(prompt, budget).await + " (llm reply unparsed → heuristic)"
        },
        _ => self.fallback.route(prompt, budget).await + " (llm classify failed → heuristic)",
     }
     // tier→model selection + budget downshift + provider fallback all reuse the existing path
  }
}
```
- The classify call uses **no tools**, tiny output; with a `claude-cli::`/`codex-cli::` or local
  `ollama::` classifier model it's $0 / free.
- Budget: still gated by run_turn's pre-routing hard-stop. The classify call itself isn't
  metered against the cap in v1 (document this; it's tiny/local/$0 in the intended setup).
- Determinism: only when opted in. Heuristic remains the default and the fallback.

### Config
```toml
[mesh]
classifier = "heuristic"          # or "llm"
classifier_model = "ollama::llama3.2"   # cheap/$0 model used only to label the tier
```

### Edge cases
| Case | Behaviour |
|---|---|
| Empty prompt | score → Trivial; LLM path skips the call, uses heuristic |
| LLM returns "Standard." / "  complex\n" / "I think standard" | parse first alpha word → tier; else fallback |
| LLM model id unset | use trivial-tier model; if none, heuristic (AC-B4) |
| LLM call slow | hard timeout (e.g. 10s) → heuristic fallback (AC-B2) |
| classifier="llm" with mock provider | mock returns canned text → parse or fallback; tests cover both |

## Definition of done — DONE
- [x] `Router::route` is `async` (`#[async_trait]`); forge-core awaits it; all mesh/core tests pass.
- [x] Heuristic uses weighted scoring (`score_prompt`); AC-A1…A8 covered. Verified live: "design a
      lock-free queue" (24 chars) → Complex; "fix typo"/"rename … in utils.rs" → Trivial; email+wire
      → Standard. Length is a capped signal; CODE_TOKENS made symbol-only to avoid prose false-positives.
- [x] `LlmRouter` (forge-core): ≤1 cheap call with a 15s timeout, tolerant `parse_tier`, falls back to
      the heuristic on error/timeout/garbage (AC-B1/B2). Reuses `HeuristicRouter::decide` for the
      pin/budget/cost-aware selection. Heuristic default makes no call (AC-B3). Verified live: real
      ollama classified "fix this typo" → trivial; `--mock` → graceful heuristic fallback.
- [x] `mesh.classifier` / `classifier_model` wired through forge-cli build_session; `--mock` still
      shows the decision; env override (`FORGE_MESH__CLASSIFIER=llm`) works.
- [x] Rationale names firing signals / notes "classified by <model>" or the fallback.
- [x] clippy -D warnings + fmt clean; full workspace green. (CI pending on push.)
