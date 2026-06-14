# ADR-0005: Persistence via rusqlite with bundled SQLite

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

FR-7 requires persisting sessions (messages, tool calls, costs, routing decisions) locally
and resuming them. The workload is single-user, single-machine, modest volume
(requirements §4). NFRs that bear on this: portability (one static binary on three OSes,
no system library dependency), footprint, and reliability (never corrupt session state).
The blueprint specifies SQLite.

Facts as of 2026-06: rusqlite 0.40.1 (2026-06-06) is a thin, complete SQLite binding with
a `bundled` feature that compiles SQLite from source into the binary (no reliance on a
system `libsqlite3`). sqlx 0.9.0 (2026-05-21) is an async, compile-time-checked SQL
toolkit supporting multiple databases.

## Options considered

1. **rusqlite (+ `bundled`)** — direct, full SQLite feature access; bundled SQLite means a
   self-contained static binary on every OS (portability win); synchronous API. Cons:
   blocking calls must be offloaded off the async runtime; no compile-time query checking;
   SQLite-only (fine — we want exactly SQLite).
2. **sqlx (SQLite backend)** — async, compile-time-checked queries, multi-DB portable.
   Cons: heavier; its SQLite driver runs on a background thread pool anyway; multi-DB
   flexibility is irrelevant for a local-first single-file store; more moving parts than
   the requirement needs.
3. **An ORM (SeaORM/Diesel)** — schema modelling conveniences. Cons: over-engineered for a
   handful of tables; added abstraction and build cost violate proportionality.

## Decision

Use **rusqlite 0.40 with the `bundled` feature**, encapsulated in a `forge-store` crate.
All DB access goes through that crate; blocking calls are isolated there and offloaded via
`tokio::task::spawn_blocking` (or a dedicated connection thread) so the async runtime is
never blocked.

## Rationale

For a single-user local store, rusqlite is the lightest fit and `bundled` directly serves
the static-binary/portability NFR (no libsqlite3 to find on the user's machine).
sqlx's headline features (async-native, compile-time checks, multi-DB) solve problems this
project doesn't have, at extra cost. Boring and proven wins.

## Consequences

- **Positive:** Self-contained binary on all three OSes; full SQLite (JSON1, FTS, WAL)
  available; minimal dependency surface; fast.
- **Negative / trade-offs accepted:** We own a small blocking-isolation pattern; queries
  are checked at runtime/test time, not compile time (mitigated by tests + migrations).
- **Follow-ups:** Define schema + a migration mechanism (embedded SQL migrations) in
  `forge-store`; enable WAL mode for crash-resilient writes (reliability NFR). A future
  code-memory graph (roadmap) can reuse this store or add its own SQLite db.
