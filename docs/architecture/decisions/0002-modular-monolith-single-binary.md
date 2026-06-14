# ADR-0002: Modular monolith shipped as a single binary (Cargo workspace)

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

Forge is a local-first CLI/TUI for one user on one machine (requirements §4). There is no
network service to scale, no multi-tenant backend. Yet the roadmap is broad (code memory,
multi-agent, MCP, marketplace) and demands clean internal seams so subsystems can be added
and swapped without core rewrites (NFR: maintainability, extensibility). The decision is
how to structure the code: one crate, a Cargo workspace of focused crates, or separate
processes/services.

## Options considered

1. **Single-crate binary** — simplest to start. Cons: as the roadmap lands, module
   boundaries erode into a tangle; nothing enforces the seams.
2. **Cargo workspace of library crates + one thin binary** — each subsystem (provider,
   router, tools, persistence, core/session, tui, config) is its own crate with an
   explicit public API; the binary wires them together. Compiles to one static
   executable. Cons: slightly more upfront ceremony (multiple `Cargo.toml`).
3. **Multi-process / service-oriented** — separate daemon, etc. Cons: massive
   over-engineering for a single-user local tool; violates proportionality and the
   single-binary NFR. Rejected outright.

## Decision

A **modular monolith**: a Cargo **workspace** of small library crates with explicit APIs,
assembled by one thin `forge` binary crate that produces a single static executable.

## Rationale

This is the 2025–2026 consensus default — start as a well-structured monolith, let crate
boundaries enforce the architecture, and extract services only if/when a real need appears
(team relay is the only plausible future service, and it would be a *separate* component).
Workspace crates give compiler-enforced seams (crate B can't reach into crate A's
internals) at near-zero runtime cost, directly serving the maintainability/extensibility
NFRs while preserving the single-binary delivery promise.

## Consequences

- **Positive:** Enforced boundaries; each subsystem testable in isolation; trivial to add
  a new provider/tool crate; still one binary to ship.
- **Negative / trade-offs accepted:** A few more `Cargo.toml` files and deliberate
  dependency-direction discipline (no cycles between crates).
- **Follow-ups:** Define the crate list and allowed dependency directions in the
  architecture doc (§8). Future hosted-relay would be a new top-level component, not a
  refactor of this one.
