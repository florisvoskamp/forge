# ADR-0009: MCP client as a dedicated crate + `External` side-effect class

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Floris Voskamp

## Context

Forge needs to drive external MCP servers (the owner's daily GitLab-MR-review workflow;
~270 MCP calls in the usage evidence) — see [mcp-client.md](../../features/mcp-client.md).
This wires **untrusted third-party servers** into a loop that can already write files and
run shell, so it is security-sensitive. Two cross-cutting questions had to be settled before
building: (1) where the client lives in the crate graph and what protocol stack it uses, and
(2) how MCP tool calls are classified for the permission broker (ADR-0008).

Forge already runs `rmcp` (the official Rust MCP SDK) on the **server** side (`forge
mcp-serve`, the CLI-bridge harness). The client side is the mirror of that.

## Options considered

**Protocol stack**
1. **Hand-roll JSON-RPC 2.0 + transports** (stdio framing, SSE/streamable-HTTP). Full
   control, but re-implements a spec we already depend on and must track as MCP evolves.
2. **Reuse `rmcp` client features** (`client`, `transport-child-process`,
   `transport-streamable-http-client`). Same SDK as the server side; battle-tested
   `initialize`/`tools/list`/`tools/call`/resources/prompts; paginated wrappers.

**Crate placement**
1. Put the client in `forge-tools`. Couples a heavy async/transport dependency into the
   leaf tool crate and inverts the dependency direction (tools would need core/config).
2. **A new `forge-mcp` crate** depending on `forge-types` + `forge-config`, with `forge-core`
   and `forge-cli` depending on it. Keeps the workspace graph acyclic.

**Registry integration**
1. Make `ToolRegistry` mutable at runtime (register/withdraw MCP tools), as the feature spec
   sketched. Touches the hot read path of every turn and the `with_core_tools` invariants.
2. **Manager-owned model:** the `McpManager` owns all MCP state (catalog, exposed set,
   connections); `Session` holds an `Option<Arc<McpManager>>` and queries it from
   `tool_specs()` / routes to it from `invoke_tool()`. `ToolRegistry` is untouched.

**Permission classification**
1. Reuse `SideEffect::Network`. But a network *read* (web fetch) is benign and auto-allowed
   in `accept-edits`; an MCP call invokes untrusted server code whose result re-enters the
   loop — a different risk.
2. **A new `SideEffect::External`** class, gated like a side effect: `default`/`accept-edits`
   ask, `plan` denies, only `bypass` auto-allows.

## Decision

Adopt option 2 in every case: a dedicated **`forge-mcp`** crate built on **`rmcp`'s client**
(stdio + streamable-HTTP), a **manager-owned** integration (no `ToolRegistry` mutation), and a
new **`SideEffect::External`** permission class for all MCP calls.

The manager surfaces a small neutral API to `forge-core`: `advertised_specs()` (meta-tools +
exposed tools), `knows_tool()`, `side_effect_of()`, `call()`, and status accessors — so
`forge-mcp` need not depend on `forge-provider`. **Deferred loading** mirrors the
`ToolSearch` mechanism Forge itself runs under: servers' tools are discovered but advertised
only via `mcp_search_tools`→`mcp_expose_tool` (plus an allowlist/eager set), keeping the
per-turn tool list bounded for large servers. The local catalog meta-tools
(`mcp_search_tools`, `mcp_expose_tool`, `mcp_list_resources`) are `ReadOnly` (no egress);
everything that round-trips a server (`mcp_read_resource`, `mcp_get_prompt`, and every real
server tool) is `External`.

## Rationale

- Reusing `rmcp` means one protocol stack for both client and server, and it tracks the spec
  for us. The in-process duplex transport also gives real client↔server tests with no child
  process.
- The manager-owned model satisfies the dynamic-exposure requirement with **zero** change to
  the per-turn read path: when no server is configured, `tool_specs()` is byte-for-byte
  unchanged and the whole path is inert.
- `External` makes "untrusted MCP server" a structural class the broker reasons about, rather
  than overloading `Network`. Combined with the allowlist and deferred loading (hostile tool
  descriptions stay out of context until surfaced), this implements the spec's threat posture
  (treat every server as untrusted by default).

## Consequences

- **Positive:** acyclic graph; no `ToolRegistry` churn; one MCP stack; MCP calls are audited
  and gated exactly like built-in tools; tokens resolve from env/keyring only (never TOML).
- **Negative / trade-offs accepted:** reconnect is **lazy + bounded** (re-established on the
  next call after a drop, not via a background task) — simpler and adequate for a single-PR
  scope; a long-idle dead server is only detected on use. Per-server `permission_mode`
  overrides, OAuth, and a distinct `McpExternal` *mode* are deferred (Could-have).
- **Follow-ups:** exposing Forge as an MCP server to *other* clients; `@`-mentionable MCP
  resources; OAuth auth-code flow.
