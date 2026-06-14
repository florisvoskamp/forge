# ADR-0008: Tool safety via switchable permission modes + per-tool rules

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

Forge tools can write files and execute shell commands on the user's machine driven by a
model's output. This is the single largest security/safety surface (threat model: an LLM
or a prompt-injected file can attempt destructive or exfiltrating actions). FR-10 (with
confirmed A-5) requires **switchable permission modes** plus fine-grained per-tool /
per-project allow-ask-deny rules, defaulting to safe behaviour, and configurable to bypass
for trusted flows.

## Options considered

1. **Single fixed policy (always ask)** — safe but unusably noisy for bulk edits.
2. **Per-tool allow/ask/deny rules only** — flexible but no fast global switch; users must
   reconfigure rules to change posture for a session.
3. **Permission *modes* (global posture) layered over per-tool rules** — a session-level
   mode sets the default posture; per-tool/per-project rules refine it. Mirrors the
   well-understood Claude Code model the user referenced.

## Decision

A **central permission broker** (in the session core) gates every side-effecting tool call.
It resolves a decision from two layers:

1. **Mode** (session-level posture, switchable at runtime and as a configured default):
   - `default` — ask before any side effect (file write/edit, shell).
   - `accept-edits` — auto-allow file writes/edits; still ask for shell commands.
   - `bypass` — auto-allow all tool actions (explicit, deliberate opt-in).
   - `plan` — read-only; deny all side effects (planning/analysis sessions).
2. **Rules** (per-tool / per-project allow | ask | deny) that refine the mode, with an
   explicit precedence: an explicit `deny` rule always wins; otherwise the mode decides;
   `plan` mode overrides any `allow`.

Read-only tools (read/search/list) never prompt. Every gated decision is recorded with the
session (observability) and surfaced through the presenter as a permission event (ADR-0004).

## Rationale

Modes give a one-switch safety posture for different workflows; rules give precision; the
`deny`-wins / `plan`-overrides precedence makes "safe by default, dangerous only on
explicit opt-in" a structural guarantee rather than a convention — directly serving the
security NFR and FR-10. Centralising the check in one broker means no tool can bypass it.

## Consequences

- **Positive:** Safe out of the box; fast posture switching; auditable decisions; a single
  chokepoint to reason about for the threat model.
- **Negative / trade-offs accepted:** `bypass` mode is genuinely dangerous — it must be a
  conscious, clearly-signalled choice (UI warning), never a silent default.
- **Follow-ups:** Define the `Tool` trait's `side_effect` classification, the rule schema
  in `forge-config`, and the path-scoping for rules (e.g. allow writes only within CWD).
  Feed this into the Phase 3 threat model.
