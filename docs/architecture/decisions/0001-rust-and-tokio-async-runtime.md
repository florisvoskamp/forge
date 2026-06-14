# ADR-0001: Rust on the Tokio async runtime

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

The product thesis (blueprint, requirements §1) demands sub-100 ms startup, a single
static binary, low memory footprint, and safe parallelism for future multi-agent
orchestration (NFR: performance, portability, footprint). The harness is I/O-bound:
concurrent streaming HTTP to model providers, subprocess execution for tools, file I/O,
and a TUI render loop — all of which must run without blocking each other.

Rust is fixed by the blueprint and is the right tool for these NFRs. The open question is
the concurrency model: a `std::thread` + channels design, or an async runtime.

Facts as of 2026-06: tokio 1.52.3 is current stable and the de-facto async runtime in the
Rust ecosystem; reqwest, sqlx, genai, and ratatui's async examples all target it.

## Options considered

1. **Tokio async runtime** — mature, ubiquitous, first-class ecosystem support
   (reqwest, genai, tower). `tokio::select!` cleanly multiplexes the TUI event loop,
   streaming responses, and timers. Cons: async colouring complexity; learning curve.
2. **`std::thread` + channels (no async)** — simpler mental model, no `.await`. Cons:
   one OS thread per concurrent model stream/tool is wasteful and clumsy; most HTTP/AI
   crates are async-first, so we'd fight the ecosystem; future fan-out to N agents scales
   poorly with thread-per-task.
3. **`async-std`** — alternative async runtime. Cons: effectively unmaintained/eclipsed
   by tokio; far smaller ecosystem. Non-starter for 2026.

## Decision

Build on **Rust (stable channel)** with the **Tokio** multi-threaded async runtime.

## Rationale

Tokio is the only option whose ecosystem directly supplies the libraries we need
(streaming HTTP, multi-provider AI client, async TUI patterns) and whose task model fits
the future multi-agent fan-out without thread explosion. The async complexity is a known,
bounded cost; the alternatives impose larger costs (ecosystem friction, poor scaling).

## Consequences

- **Positive:** Idiomatic access to reqwest/genai/sqlx; clean concurrency for streaming +
  TUI + tools via `select!`; scales to parallel agents later.
- **Negative / trade-offs accepted:** Async function colouring; blocking work (e.g.
  rusqlite, subprocesses) must be deliberately offloaded to `spawn_blocking` / dedicated
  threads.
- **Follow-ups:** Pin an MSRV and test it in CI (Phase 4). Establish a convention for
  where blocking calls are isolated (persistence layer, tool executor).
