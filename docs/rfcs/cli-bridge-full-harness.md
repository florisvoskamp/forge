# RFC: CLI-bridge as a full Forge-harness agent

| Field | Value |
|-------|-------|
| Status | DRAFT |
| Author | Forge |
| Created | 2026-06-15 |
| Implements | follow-up to PR #26/#29 (CLI bridge) |
| Supersedes | the "tool-disabled text backend" design in provider-integrations.md (Part B) |

## Summary

The CLI bridge (`claude-cli::` / `codex-cli::`) currently runs the subscription model
**tool-disabled**, parsing only final text. That is useless for real work — the model
can't read files, use tools, or show its reasoning. This RFC redesigns the bridge so the
subscription model becomes *just the brains* while **Forge owns the harness**: Forge serves
its own tools to the CLI over an in-process MCP server (the MCP tool handler doubles as the
permission boundary), and Forge parses the CLI's full event stream so the TUI shows streamed
**thinking + response text + tool activity**. As Forge later gains an MCP client and skills,
they surface as Forge tools and flow through the bridge automatically.

## Problem statement

A subscription-billed turn through the bridge today (`claude -p --tools "" --max-turns 1`):
- **cannot use any tool** — `--tools ""` disables them, so "explain this repo" gets a guess,
  not an answer (the model literally can't read a file);
- **shows no reasoning** — only the final `result.result` text is surfaced; the live
  `thinking` and `tool_use` events are discarded;
- **bypasses the Forge harness** — none of Forge's tools, permission engine, or (future)
  MCPs/skills participate.

So the differentiator ("use your subscription through Forge") delivers a crippled agent.
The user's verdict: *"just plain text integration of claude/codex is useless."*

### Non-goals
- Replacing genai/API providers — the bridge is one provider among many.
- Building Forge's MCP **client** or skills subsystem (separate roadmap items). This RFC only
  ensures they plug in later as ordinary Forge tools.
- Making the bridge ToS-compliant beyond what PR #26 already documented (unchanged: Forge
  never handles the OAuth token; the official CLI owns its auth).
- Sharing one long-lived CLI process across Forge turns (each `complete()` is one CLI session;
  cross-turn reuse is a later optimisation).

## Background and context

- `Provider::complete(model, messages, &[ToolSpec], &mut TextSink) -> ModelResponse` is the
  seam. Today the bridge ignores the tools and returns `tool_calls: []`.
- `forge-core` runs its OWN agent loop (`MAX_STEPS` model↔tool round-trips) for genai
  providers. The bridge inverts this: **the CLI runs the loop**, so a bridge `complete()` is a
  single call that internally drives the CLI's multi-tool agentic turn and streams everything.
  (See "Reconciling two loops".)
- Permission today: `forge-core::permission::decide(mode, side_effect, tool, args, rules)`
  gates each Forge tool call, surfaced via `Presenter::confirm`.

### Verified facts (live, claude 2.1.177 / codex-cli 0.130.0)

| Capability | Status |
|---|---|
| claude `--mcp-config <json>` + `--strict-mcp-config` | ✅ load only our MCP servers |
| claude `--allowedTools "mcp__forge__*"` / `--disallowedTools` | ✅ disable built-ins, allow only Forge tools |
| claude `--input-format stream-json` + `--output-format stream-json` | ✅ bidirectional, multi-turn |
| claude `--permission-mode bypassPermissions` | ✅ (Forge's MCP handler gates instead) |
| claude `--permission-prompt-tool` | ❌ **not a flag** → use the MCP-handler-as-boundary trick |
| claude stream-json `thinking` / `tool_use` / `tool_result` / `result` events | ✅ confirmed live |
| codex `mcp` (external servers) + `mcp-server` + `exec --json` | ✅ flags exist |
| codex `exec --json` reasoning-event schema | ❓ unverified — **spike** |
| `rmcp` 1.7.0 (official Rust MCP SDK, server+client, stdio/streamable-http) | ✅ on crates.io |

## Proposed solution

### High-level design

```
forge-core turn
  └─ provider.complete("claude-cli::…", messages, forge_tools, on_event)
        │
        ├─ (1) start an in-process Forge MCP server  ──serves──▶ ToolRegistry
        │         (rmcp, loopback http, per-session token)        (read_file, shell, …)
        │         each tool handler: permission::decide → run → result
        │
        ├─ (2) spawn `claude --input-format stream-json --output-format stream-json
        │         --mcp-config <forge server> --strict-mcp-config
        │         --allowedTools "mcp__forge__*" --permission-mode bypassPermissions`
        │
        ├─ (3) write the transcript as stream-json on stdin
        │
        └─ (4) read stdout events → map to PresenterEvents:
                 thinking   → Reasoning(delta)      [NEW]
                 text       → AssistantDelta(delta)
                 tool_use   → ToolStart{name,args}
                 tool_result→ ToolResult{name,ok,summary}
                 result     → usage (cost_usd = 0, subscription)
        ↑ when claude calls mcp__forge__shell, the call lands back in (1) IN-PROCESS,
          goes through the permission broker + presenter, runs the real Forge tool.
```

The model never touches a tool directly: **every tool/side-effect is a Forge MCP tool**, so
Forge's permission engine and (future) MCP/skill-derived tools are the only capabilities.

### Detailed design

**1. In-process Forge MCP server (`forge-mcp` or a module in forge-core).**
- Built on `rmcp` with the **streamable-http** transport bound to `127.0.0.1:<ephemeral>`,
  protected by a per-session bearer token (passed in the `--mcp-config` header). Loopback +
  token so no other local process can drive Forge's tools.
- Exposes each `ToolRegistry` tool as an MCP tool named `forge__<tool>` (claude sees
  `mcp__forge__<tool>`). `tools/list` is generated from the registry's `name`/`description`/
  `schema`; `tools/call` dispatches into the registry.
- **Permission boundary in the handler:** before executing, the handler calls
  `permission::decide(mode, tool.side_effect(), name, args, rules)`. `Deny`/unconfirmed →
  return an MCP *tool error* (`isError: true`, text explains the denial) that the model reads
  and adapts to. `Ask` → `Presenter::confirm` (the existing TUI prompt). This replaces the
  missing `--permission-prompt-tool` flag — **the server is the gate**, and because it runs
  in-process it has the real broker + presenter + emits `Diff`/`ToolStart`/`ToolResult`.

**2. CLI invocation (claude).**
```
claude --print
  --input-format stream-json --output-format stream-json --verbose
  --mcp-config '{"mcpServers":{"forge":{"type":"http","url":"http://127.0.0.1:PORT/mcp",
                 "headers":{"Authorization":"Bearer <token>"}}}}'
  --strict-mcp-config
  --allowedTools "mcp__forge__*"
  --permission-mode bypassPermissions
  [--model <bare>]
```
`--strict-mcp-config` ignores the user's own MCP servers (deterministic), `--allowedTools`
limits the model to Forge tools (no claude built-ins), `bypassPermissions` stops claude from
prompting (Forge gates inside the handler). Forge still **never** sets any auth env/flag.

**3. Driving the session.** Write the assembled transcript as a stream-json user message on
stdin. For v1 a single user turn is enough (Forge's transcript already carries history);
multi-turn stdin streaming is a later enhancement. Enforce the existing timeout + process-group
kill.

**4. Event → Presenter mapping.** Parse each NDJSON line (field-tolerant, like today):

| CLI event | PresenterEvent |
|---|---|
| `{"type":"thinking","thinking":t}` | **`Reasoning(t)`** (NEW) |
| `assistant` content `{type:text}` | `AssistantDelta(text)` |
| `assistant` content `{type:tool_use,name,input}` | `ToolStart{name,args}` (informational; the tool already runs via MCP) |
| `user` content `{type:tool_result,…}` | `ToolResult{name,ok,summary}` |
| `result{usage,total_cost_usd}` | `Usage{…, cost_usd:0}` + final content |
| `result{is_error,subtype}` | `ProviderError` (with the existing actionable message) |

`ModelResponse.content` = concatenated assistant text; `tool_calls` stays empty (the CLI loop
already executed them — Forge does not re-run them).

### API changes (internal)

- **`forge-tui::PresenterEvent::Reasoning(String)`** — NEW. Streamed thinking/reasoning text.
  Headless prints it dim/prefixed; the ratatui TUI renders it in a muted "thinking" style
  above the answer (collapsible later). All existing presenters get the arm (no behaviour
  change for non-bridge turns, which simply never emit it).
- `Provider::complete` signature unchanged. The bridge now *uses* the `&[ToolSpec]` it's given
  (to populate the MCP server's tool list) instead of ignoring it.
- `forge-tools::ToolRegistry` gains a way to enumerate `(name, description, schema, side_effect)`
  for the MCP `tools/list` (it already holds the tools; add an accessor).

### Reconciling two loops

For genai providers, forge-core runs the model↔tool loop. For the bridge, the CLI runs it.
To avoid a double loop: when the routed model is a CLI bridge, forge-core treats the single
`complete()` as the **whole** agentic turn — it does not enter its own `MAX_STEPS` tool loop
(the response comes back with `tool_calls: []` and final text, which already terminates the
loop today). The Forge tools still ran — inside the MCP server — and their `ToolStart`/
`ToolResult`/`Diff` events already reached the presenter. No core-loop change is required
beyond confirming an empty `tool_calls` ends the turn (it does).

### Rollout plan (phased; feature-flagged)

A `mesh`/bridge config flag `bridge_mode = "text" | "harness"` (default `text` until Phase 2
is proven) lets this land incrementally without regressing the working bridge.

- **Phase 1 — rich streaming (low risk, high visible value).** Stop tool-disabling; run claude
  as a full agent (its own tools, project cwd, `--permission-mode acceptEdits` or default) and
  **parse + stream the full event stream** (thinking → new `Reasoning` event, text, tool
  activity) into the TUI. Proves stream parsing + the `Reasoning` event end-to-end. Tools are
  still claude's here — an explicit, documented intermediate.
- **Phase 2 — Forge tool-bridge.** Add the in-process `rmcp` server exposing `ToolRegistry`
  with the permission-boundary handler; switch the argv to `--strict-mcp-config
  --allowedTools "mcp__forge__*" --permission-mode bypassPermissions`. Now tools + permissions
  are Forge's. Gate behind `bridge_mode = "harness"`.
- **Phase 3 — codex parity + extensibility.** Bring codex to the same model via `codex mcp`
  config + `exec --json` (after the reasoning-schema spike). Confirm that future Forge MCP-client
  tools and skills, being ordinary `ToolRegistry` entries, appear automatically.

### Spike checklist (must pass before committing Phase 2)

1. Stand up a minimal `rmcp` streamable-http server on loopback exposing one tool (`echo`).
2. Run `claude --print --output-format stream-json --mcp-config <that server>
   --strict-mcp-config --allowedTools "mcp__forge__echo" --permission-mode bypassPermissions`
   with a prompt that forces the tool; **confirm** claude calls it and we see the call in our
   server + a `tool_use`/`tool_result` in the stream.
3. Confirm `thinking` events appear and are parseable in the same run.
4. Confirm an MCP-handler error (simulated deny) is surfaced to the model and it adapts.
5. Measure added latency of the loopback round-trip per tool call.

## Alternatives considered

### Alternative A: keep the tool-disabled text backend
**Why rejected:** the status quo — the user calls it useless; the model can't read files or
act.

### Alternative B: let the CLI use its OWN tools/MCPs/skills, Forge just renders
**Description:** run `claude -p` as a full agent with its native tools; Forge only parses/streams.
**Why rejected:** not the Forge harness — Forge's permission engine, rules, and (future)
MCP/skill set don't apply; side effects bypass Forge's gate; behaviour diverges between bridge
and API providers. (Adopted *temporarily* as Phase 1 only, clearly flagged, to de-risk the
streaming half.)

### Alternative C: in-process MCP tool-bridge (CHOSEN)
**Description:** the design above — Forge serves its tools via MCP; the handler is the
permission boundary; Forge parses the full stream.
**Why chosen:** the only option where the subscription model runs under Forge's *own* tools and
permissions, extensible to future MCP/skills, while still streaming thinking + text. The
verified CLI flags + `rmcp` make it buildable.

### Alternative D: do nothing
**Why rejected:** the bridge stays a demo, not a usable way to spend a subscription.

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Multi-turn stream-json **input** control protocol underdocumented | Med | Med | v1 sends a single user turn (history already in transcript); spike the input format; multi-turn is a later enhancement |
| codex `exec --json` reasoning/tool schema differs/undocumented | High | Med | Phase 3 + a dedicated codex spike; claude ships first; field-tolerant parser |
| In-process http server lifecycle (port, startup race, shutdown) | Med | Med | ephemeral loopback port, readiness check before spawning claude, drop-guard shutdown, kill child on drop |
| Localhost MCP server is an unauthenticated tool-exec surface | Med | **High** | bind 127.0.0.1 only + per-session random bearer token in the `--mcp-config` header; refuse non-loopback; short-lived with the turn |
| Permission prompt latency inside an agent loop (model waits) | Med | Low | non-interactive modes pre-decide via rules; only `Ask` blocks; same UX as today's confirm |
| Double agent-loop confusion (core loop + CLI loop) | Low | Med | bridge `complete()` returns empty `tool_calls` → core loop terminates; documented in "Reconciling two loops" |
| claude version drift changes event/flag names | Med | Med | field-tolerant parsing (already the pattern); pin verified versions in docs; degrade to text mode on parse-empty |
| `rmcp` API churn (1.x) | Low | Low | pin minor; thin wrapper around the registry so an SDK swap is local |

## Security considerations

- **New attack surface:** an in-process MCP server that executes Forge tools (incl. `shell`).
  Mitigations: bind **loopback only**, require a **per-session random bearer token** (the only
  client that knows it is the claude child Forge spawned), tear it down when the turn ends.
- **Permission integrity:** because the handler runs the *real* `permission::decide` + builtin
  safety denylist before executing, the model cannot exceed the user's configured permissions —
  even though claude runs with `bypassPermissions` (that only stops claude's *own* prompt; it
  does not bypass Forge's gate, which is a different process boundary).
- **Token boundary unchanged:** Forge still never reads/sets the subscription OAuth token.
- **Prompt-injection:** a malicious repo could try to make the model call destructive tools;
  the builtin denylist (`rm -rf`, secret reads, etc.) and rules still apply at the handler.

## Operational considerations

- New runtime dependency on `claude`/`codex` being installed + authenticated (already true).
- New: an ephemeral local TCP listener per bridge turn — document firewall/sandbox implications.
- Added latency: one loopback HTTP round-trip per tool call (expected sub-ms locally).

## Performance considerations

- Streaming improves *perceived* latency (thinking/text appear live vs one final blob).
- Per-tool loopback overhead is negligible vs model latency. Measure in the spike (risk table).

## Open questions

1. **Transport:** streamable-http (needs a port + token) vs stdio (claude spawns `forge
   __mcp-serve` as a child — simpler auth, but that child is a *separate process* without the
   parent's presenter/broker). Leaning http-in-process so the handler shares the live session.
   Confirm rmcp's in-process http server ergonomics.
2. Should `bridge_mode` be per-provider (claude vs codex) or global? (Proposed: global flag,
   per-provider override later.)
3. How to render streamed `Reasoning` in the inline-scrollback TUI — always-on dim text, or
   collapsed behind a toggle? (Proposed: dim, shown live, not persisted to scrollback.)
4. Multi-turn within one bridge `complete()` — needed for v1, or is single-shot-with-history
   sufficient? (Proposed: single-shot; transcript carries history.)

## Decision log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-06-15 | Permission via MCP-handler, not a CLI flag | `--permission-prompt-tool` doesn't exist; in-process handler is a cleaner, stronger gate |
| 2026-06-15 | Phased rollout behind `bridge_mode`, Phase 1 = streaming | de-risk the high-value streaming half before the MCP-server build |

## References

- docs/features/provider-integrations.md (Part B — current bridge)
- crates/forge-provider/src/cli_provider.rs
- rmcp (Rust MCP SDK) — https://crates.io/crates/rmcp
- ADR-0003 (Provider abstraction), ADR-0008 (permission modes)
