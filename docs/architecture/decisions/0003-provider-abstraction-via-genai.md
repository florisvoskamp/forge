# ADR-0003: Provider abstraction — own `Provider` trait, `genai` as the v0.1 backend

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

FR-3 requires a provider-agnostic model interface covering Anthropic, OpenAI, and Ollama
for v0.1, with streaming and normalized tool/function calling, and the roadmap adds many
more providers. Building and maintaining hand-rolled HTTP adapters for each provider's
evolving API (auth, streaming SSE formats, tool-call schemas, error shapes) is a large,
perpetual maintenance burden for a solo/OSS project (constraint §6: solo author).

Facts as of 2026-06:
- `genai` v0.6.5 (released 2026-06-09; dual MIT/Apache-2.0; ~800★, actively maintained,
  889 commits) is a multi-provider Rust client covering 25+ providers including Anthropic,
  OpenAI, Ollama, Gemini, Groq, xAI, DeepSeek, Cohere, Bedrock, and OpenAI-compatible
  custom endpoints. It supports streaming, typed tool calling, and per-request model
  selection.
- Hand-rolling would use `reqwest` 0.13.4 + `serde` directly.

The risk with adopting genai: it is a 0.x crate (API churn possible) and its unified
abstraction may not expose every provider-specific feature we eventually want (e.g.
Anthropic prompt caching, extended thinking, fine-grained token accounting).

## Options considered

1. **Hand-rolled adapters per provider (reqwest + serde)** — full control, no third-party
   abstraction risk. Cons: large, perpetual maintenance; we re-implement what genai
   already does well; slow path to provider breadth (a roadmap goal).
2. **Use `genai` directly throughout the codebase** — fastest. Cons: hard lock-in to its
   types across the whole app; a breaking genai change ripples everywhere; can't escape to
   a native adapter for a provider that needs special features.
3. **Own `Provider` trait with `genai` as one backing implementation** — define Forge's
   own minimal `Provider` trait (chat, stream, tool-call, usage/cost), implement it once
   over genai for v0.1 breadth, and keep the seam so we can add native adapters later for
   providers needing special features. Cons: one extra abstraction layer to maintain.

## Decision

Define a Forge-owned **`Provider` trait** in a `forge-provider` crate. Ship a single
**`GenAiProvider`** implementation wrapping `genai` 0.6.x for v0.1 (covering Anthropic,
OpenAI, Ollama out of the box). All other crates depend only on the trait, never on
`genai` types.

## Rationale

This captures genai's breadth and maintenance savings now (directly serving FR-3 and the
multi-provider roadmap on a solo budget) while the owned trait is the seam that controls
lock-in (NFR: extensibility, maintainability). If genai churns or a provider needs native
features, we add a native adapter behind the same trait without touching the router, TUI,
or session core. Boring-and-proven where it's cheap, control where it matters.

## Consequences

- **Positive:** Three+ providers working in v0.1 with little provider code; one place to
  normalize streaming/tool-calls/usage; provider breadth roadmap becomes "configure
  genai" not "write an adapter"; lock-in contained at one crate.
- **Negative / trade-offs accepted:** An extra mapping layer (genai types → Forge types);
  provider-specific niceties (e.g. Anthropic caching) are deferred until we add a native
  adapter; we track a 0.x dependency and must pin it.
- **Follow-ups:** Keep the `Provider` trait minimal and provider-neutral. Pin genai to a
  compatible range. Record cost/usage extraction needs so the trait surfaces token counts
  for the Model Mesh (ADR-0006).
