# ADR-0006: Model Mesh — rule-based, pluggable routing engine

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

The Model Mesh (FR-4) is the product's killer differentiator: every task is routed to the
cheapest model that can do it well, under a budget (FR-5). The routing decision must be
**deterministic, transparent, configurable, and add no cost/latency** by default
(confirmed assumption A-2). It must also surface *why* it chose a model (NFR:
observability) and respond to budget pressure (FR-5).

The hard question is how the tier is decided without itself costing a model call.

## Options considered

1. **Rule-based + heuristic classifier** — derive a task tier (trivial / standard /
   complex) from cheap local signals: prompt length/token estimate, presence of code vs
   prose, target file size, tool-only vs reasoning, explicit user hints/overrides, and
   user-defined rules. Map tier → configured model, adjusted by remaining budget. Cons:
   heuristics are approximate; needs tuning.
2. **Cheap-LLM classifier** — ask a tiny model to label difficulty before routing. More
   adaptive. Cons: adds a call (cost + latency + non-determinism) to *every* task — exactly
   what A-2 rejects as the default.
3. **Always-frontier / single-model (no mesh)** — trivial. Cons: defeats the entire
   purpose of the product.

## Decision

Implement the Model Mesh as a **rule-based + heuristic routing engine** in a `forge-mesh`
crate, structured behind a `Router` trait so a classifier is **pluggable**. v0.1 ships the
deterministic heuristic router. An optional cheap-LLM classifier is an opt-in `Router`
implementation added later (A-2), never on by default.

Routing inputs: task signals (size, kind, tool-vs-reason, user hint) + user-configured
rules + tier→model map + live budget state. Output: a chosen model **plus a recorded
rationale** (which rule/signal fired) persisted with the session and shown in the TUI.

## Rationale

A deterministic, zero-extra-cost router is the only design consistent with A-2, the
cost-correctness NFR, and the "you see what each task costs and why" UX. The `Router`
trait keeps the smarter-classifier roadmap open without committing to its costs now.
Recording the rationale makes routing auditable (observability) and tunable.

## Consequences

- **Positive:** No added latency/cost per task; fully deterministic and configurable;
  transparent decisions (good UX + debuggability); budget caps integrate directly into the
  decision; classifier upgrade path preserved.
- **Negative / trade-offs accepted:** Heuristics need iteration to match human judgement;
  some tasks will be mis-tiered until rules are tuned (mitigated by user overrides + pin).
- **Follow-ups:** Define the signal set and default rule pack; define budget→tier
  degradation policy (warn / downshift / block) for FR-5; expose per-decision rationale in
  the presenter events (ADR-0004).
