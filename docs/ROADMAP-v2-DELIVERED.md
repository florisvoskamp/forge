# Forge v2.0.0 Roadmap — Delivery Record

Every line of [`ROADMAP-v2.md`](./ROADMAP-v2.md) mapped 1:1 to the PR that delivered it.
Assembled `main` verified green: `cargo fmt --check` clean, `cargo clippy --workspace
--all-targets --all-features -- -D warnings` clean, `cargo test --workspace` = 1163 passed /
24 ignored. Every PR also passed full CI (Linux + macOS + Windows + cargo-audit + cargo-deny +
release-build) before merge.

## P0 — done
| Item | PR |
|---|---|
| Non-PTY shell hang on backgrounded process | #360 |
| `forge run`/`nl` onboarding reach + no-model message → `forge setup` | #383 |
| Runtime custom OpenAI-compatible endpoints (LM Studio/vLLM/llama.cpp) | #382 |
| MCP OAuth Dynamic Client Registration (RFC 7591) | #385 |

## P1 Robustness — done
| Item | PR |
|---|---|
| `retry-after` parser panic on malformed 429 body | #360 |
| Truncated-but-closed stream returned as complete (phantom success) | #360 |
| `web_fetch` uncapped body → OOM | #360 |
| Read-then-write DEFERRED txns → `BEGIN IMMEDIATE` | #364 |
| Single-writer WAL busy-retry on critical writes | #364 |
| `import_portable_metadata` SQL column-name validation | #360 |

## P1 UX — done
| Item | PR |
|---|---|
| Slash/keybind discoverability cue + `/keys` overlay | #383 (+#362) |
| Palette `usage`/arg hints + enum completion | #383 |
| Inline streaming render via `streaming_edge` | #383 |
| `Error` presenter variant — red failure banner | #383 |
| anyhow `Display` printer in `main()` + `forge doctor` hint | #383 |
| `/config` reaches hooks / MCP / permissions | #383 |

## P1 Ecosystem — done
| Item | PR |
|---|---|
| crates.io readiness (`cargo install forge-agent`) + AUR + Scoop | #363 |
| Homebrew Linux ARM | #363 |
| `forge import claude/codex` imports permissions + hooks + MCP | #388 |
| Claude-Code-compatible hooks + missing lifecycle events | #388 |
| `forge run --output-format stream-json` (NDJSON) | #388 |
| MCP client sampling / roots / elicitation + non-text content | #385 |
| Azure / Bedrock / Vertex + Together / Fireworks / Perplexity | #382 (5) + #386 (Azure) |
| Skills marketplace + `update` + version pinning + generic-git | #388 |
| cargo-audit + cargo-deny CI + Dependabot + CODEOWNERS | #363 |

## P2 Robustness — done
| Item | PR |
|---|---|
| `UNIQUE(session_id, seq)` + atomic seq allocation | #364 |
| In-process file tools workspace confinement | #381 |
| Pre-write snapshot failure surfaced (warns `/undo` can't restore) | #381 |
| Centralized path extraction across all path-arg keys | #381 |
| Dropped MCP connection reaps its subprocess | #385 |
| MCP manager `parking_lot::Mutex` (no poison cascade) | #385 |
| `set_var` checkpoint handoff → explicit per-child `Command` env | #389 |
| `set_tasks` persistence failure surfaced | #381 |
| reqwest connect-timeout + discovery-call timeout | #382 |
| Schema-version gate (`PRAGMA user_version`) + versioned migrations | #364 |
| File watcher prunes deleted files (no phantom symbols) | #364 |
| Store retention + VACUUM + `result_json` cap | #364 |
| Blocking `std::fs` off the async turn path | #381 ¹ |

## P2 UX — done
| Item | PR |
|---|---|
| Unify `run` vs `chat` resume/TUI conventions | #390 |
| Standardize one `--scope` enum everywhere (back-compat `--project`) | #390 |
| Consolidate overlapping commands (aliases + cross-refs) | #390 |
| Headless progress heartbeat + graceful Ctrl-C | #383 |
| `--model` validation without a catalog | #383 |
| Up-arrow respects a multiline draft | #383 |
| Width-aware truncation | #383 |
| Diff context contrast | #383 ² |
| Model-id consistency (`model_short`) | #383 |
| Reasoning collapsed-by-default (discoverable) | #383 |

## P2 Ecosystem — done
| Item | PR |
|---|---|
| genai typed-error classification + per-provider contract tests | #382 |
| Legacy SSE MCP transport + `tools/list_changed` | #387 (SSE) + #385 (list_changed) |
| Multi-secret MCP import | #388 |
| Forge-as-MCP-server HTTP transport (bearer auth) | #387 |
| Refresh CONTRIBUTING.md / SECURITY.md for post-1.0 | #363 |
| Shell completions + man page | #363 |
| Release-profile build on PR | #363 |
| JSON-Schema for config + stability policy | #363 |

## Test-coverage closeouts — done
| Item | PR |
|---|---|
| forge-mcp tests (poisoning, subprocess cleanup, reconnect, DCR) | #385 |
| forge-store concurrent-writer / busy-snapshot / dropped-write tests | #364 |
| forge-lsp coverage (JSON-RPC, diagnostics, lifecycle, URI) | #365 |
| Adversarial-input tests (retry-after, web_fetch) | #360 |

## Beyond the roadmap (delivered in the same wave)
- **Security**: the new cargo-audit gate immediately caught + fixed two real CVEs — quinn-proto
  RUSTSEC-2026-0185 (7.5, remote memory exhaustion) and an anyhow unsoundness (#380).
- **Three real cross-platform production bugs** surfaced by the new tri-platform CI and fixed:
  Windows `path_to_uri` emitted malformed `file://` URIs (#365); the lattice deleted-file prune
  keyed on a non-canonical path so macOS/Windows users kept phantom symbols (#364).
- **Dependency hygiene**: 5 safe bumps + 9 major upgrades (tree-sitter ×5, toml 1.1,
  pulldown-cmark 0.13, clap_mangen 0.3, sha2 0.11) merged green (#391).
- **Off-thread watcher reindex** (the one store follow-up #364 flagged) — #386.
- **README**: best-in-class marketing rewrite + recorded demo GIF + sourced competitive
  comparison (#361).
- **Keybinds**: configurable keybind system + interactive configurator + mid-turn `skip_model` /
  `tier_up` / `tier_down` / `/reload` (#362).

## Honest notes (delivered, with documented limits — not undone items)
1. Two tiny cached reads (`.git/HEAD`, `AGENTS.md`) kept synchronous on purpose: making them
   async breaks the spawned-turn `Send` bound and the abort-before-persist invariant
   (compile-/test-proven). Everything else on the async path moved to `tokio::fs`/`spawn_blocking`.
2. Diff context contrast was already correct on inspection; a regression guard was added.
- **crates.io**: packaging is verified (`cargo publish --dry-run`); the actual publish is a
  release-time action. `cargo install forge-agent` is the supported verb (bare `forge` needs the
  crate name reserved first).
- **`stop` / `subagent_stop` hooks** fire and report a block decision but do not yet enforce
  turn-continuation (observe-only MVP) — #388.
- **SSE MCP transport** is a hand-rolled client: no rmcp version (incl. 2.0.0) ships a standalone
  SSE client, confirmed against the crates.io index — #387.
