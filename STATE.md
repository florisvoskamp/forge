# Forge — STATE (session 4, night of 2026-06-27, deadline 10:00 CEST)

## Goal: make Forge demonstrably THE BEST harness, PROVEN, all dimensions. Ultracode, no usage cap.

## Shipped this session: 9 PRs (#283–#291), v0.4.44→v0.4.52. Releases: v0.4.48 + v0.4.50 PUBLISHED;
## v0.4.52 release pending #291 merge (covers the P0 data-loss fix #290/v0.4.51 — MUST release).

### Robustness / the critical regression
- #283 (v0.4.44): bridge prose-tool-call recovery + reverted batch nudge — fixes the 553x spiral
  (live-proven 0 spirals). #284 (v0.4.45): P0 <parameter> panic + mutex-brick + double-exec guard.
- #285 (v0.4.46): concurrent-batch failure-loop FIX + guard tests + conformance proof.
- #286 (v0.4.47): bridge stream-resilience tests + forge-lsp docs.
- #288 (v0.4.49): autofix iteration-cap test — every run-loop guard now deterministically tested.

### Proof
- #287 (v0.4.48): docs/harness/why-forge-is-a-better-harness.md — honest, test-backed case (linked from
  README). results.md §2 conformance table = every guard. 320+ deterministic harness tests.

### Diagnostics / UX (audit 2, sonnet verify-first)
- #289 (v0.4.50): 5 fixes — silent direct give-up now warns; oscillation msg says "alternating";
  stdin-write failure surfaced; setup hint dedup; plain-mode message fixed.

### Bug-hunt (audit 3, verify-first — 6 REAL bugs, all fixed)
- #290 (v0.4.51): **P0 DATA LOSS** — /undo after a compacted resume wiped pre-compaction survivors
  (self.seq=count not MAX+1; rewind_to used transcript index as DB seq). Fix: next_seq_for_session +
  index->seq offset (0 when not compacted). + doom-loop nudge dropped on concurrent batch path.
- #291 (v0.4.52, automerge in flight): in-band rate-limit now retryable (failover); record_session(None)
  clears stale id; /mesh explain uses routed tier; fallback rationale reports real reason.

## Method: 4 parallel verify-first audits (robustness / UX / bug-hunt core / bug-hunt un-audited crates).
## LESSON: many audit-1 findings were FALSE POSITIVES (haiku, guessing). Sonnet + "trace before claiming"
## + "verified flag" gave high signal (audits 2/3 found real bugs). Micro-perf findings = sub-ms (model
## round-trip dominates) — dropped honestly, not padded.

## Honest standing: robustness — demonstrably MORE robust than a naive CLI bridge (proven, runnable).
## resolve — modestly ahead (N=20: 11 vs 9). efficiency — PARITY, not a win (stated plainly). The
## batch-tools efficiency attempt backfired (spiral) and was reverted. No overclaiming.

## Release hygiene: bump every PR with `cargo update --workspace`; run EXACT CI clippy before push:
## `cargo clippy --locked --all-targets --all-features` (per-crate w/o flags misses field_reassign +
## Windows cfg(unix) dead-code). CI guard `clippy --locked` added #282.

## Open: bug-hunt 4 (tools/index/mcp/skills) running. P1 stream-json transport = the real efficiency
## fix (big/risky, deferred). Forge is MATURE — remaining gains incremental or risky; prioritize SAFE.

## UPDATE (session 4 continued — bug-hunt waves, #287–#295, v0.4.48–0.4.56)
13 PRs total this session. After the robustness/diagnostics work, ran 4 verify-first multi-agent
bug-hunts (sonnet, "trace before claiming") that found 15 REAL bugs — all fixed + tested:
- #290 (v0.4.51): **P0 DATA LOSS** — /undo after compacted resume wiped survivors (self.seq + rewind_to
  index-vs-seq); + doom-loop nudge dropped on concurrent batch.
- #291 (v0.4.52): in-band rate-limit retryable (failover); record_session(None) clears stale id;
  /mesh explain routed-tier; fallback rationale real reason.
- #292 (v0.4.53): read_file inverted-range PANIC; strip_ansi OSC leak.
- #293 (v0.4.54): MCP lazy-reconnect permanently broken after drop; dotfile source never reindexed;
  frontmatter BOM leak.
- #294 (v0.4.55): **lattice PageRank + repo-map cross-repo contamination** (unscoped global queries).
- #295 (v0.4.56): stdio MCP server's extra secret env vars silently dropped.
Bug-hunt method WORKS: sonnet + verify-first + reproducer-required = high signal (15 real bugs vs
audit-1 haiku's mostly-false-positives). Releases: v0.4.48/0.4.50/0.4.52 published; v0.4.56 pending.
A 5th bug-hunt (compaction/snapshot/subagent/cli) is running.
