# Feature: Provider Integrations (multi-provider matrix + CLI-bridge)

Status: designed · Owner: Forge · Depends on: ADR-0003 (Provider abstraction), ADR-0006
(Model Mesh), ADR-0007 (config/secrets), FR-5 (budget cap).

This spec covers two parts that ship together because they share one seam (the `Provider`
trait + the `provider::model` routing string):

- **Part A — Compliant multi-provider matrix (BYOK).** Add Google Gemini, xAI (Grok),
  DeepSeek, and OpenRouter to the existing Anthropic / OpenAI / Ollama set, all via
  bring-your-own-key. Zero ToS risk.
- **Part B — CLI-bridge provider.** A ToS-defensible way to use a Claude Pro/Max or
  ChatGPT subscription *without Forge ever touching the OAuth token*: shell out to the
  user's already-authenticated official CLI (`claude`, `codex`) as a text-completion
  backend.

---

## 0. Why this, why now (and the hard constraint that shaped it)

The user's original ask was "support OAuth login so the harness can run on my Claude
Pro/Max and ChatGPT subscriptions." **That path is now closed by vendor policy** and the
spec is built around that verified fact:

- **Anthropic, effective 2026-02-20** (Legal/Compliance doc): *"Using OAuth tokens
  obtained through Claude Free, Pro, or Max accounts in any other product, tool, or
  service — including the Agent SDK — is not permitted."* Enforcement reserved "without
  prior notice" → account-ban risk. *(High confidence: two independent reports + Claude
  Code's own auth doc agree.)*
- **OpenAI:** "Sign in with ChatGPT" ships only inside Codex. As of **2026-04-04**,
  billing enforcement means third-party traffic no longer draws from the subscription —
  it bills as overage. So the token-extraction workaround both violates OpenAI's
  "don't power third-party services" term *and* no longer saves money. *(High confidence
  on billing enforcement + Codex-only; medium on the exact proxy mechanism.)*

Conclusion: **there is no ToS-compliant way to bill a subscription from a third-party
harness by handling its OAuth token.** Part A is therefore the safe core. Part B is the
one remaining defensible angle — delegate to the *official CLI as a subprocess*, so the
credential never leaves the sanctioned tool (see §B-ToS for the honest risk analysis).

---

# PART A — Multi-provider matrix (BYOK)

## A1. Problem — JTBD

> When I configure Forge's Model Mesh, I want to route any tier to **any major provider's
> models with my own API key**, so I can pick the cheapest capable model across the whole
> market — not just the three providers hard-wired today.

Who's affected: every Forge user. Model Mesh's entire value proposition ("route each task
to the cheapest capable model under a budget") is only as good as the set of models it can
reach. Today that set is Anthropic + OpenAI + Ollama. DeepSeek and Gemini Flash are among
the cheapest capable models in 2026; OpenRouter is a single key onto hundreds of models —
both are high-leverage for routing.

**Is this the right feature?** Yes — it's the literal precondition for the mesh to mean
anything. Simpler alternative considered (rely on genai's name-inference alone): rejected,
it's fragile (ambiguous model names) and can't express OpenRouter's custom base URL.

## A2. Scope — user stories (MoSCoW)

**Must have**
- As a user, I can set `complex = "gemini::gemini-2.5-pro"` (or `xai::`, `deepseek::`,
  `openrouter::…`) in `mesh.models` and have that tier route there.
- As a user, I can run `forge auth gemini` / `xai` / `deepseek` / `openrouter` to store a
  key in the OS keyring, or set the provider's env var.
- As a user, OpenRouter routes through its OpenAI-compatible endpoint
  (`https://openrouter.ai/api/v1`) with my key as a bearer token.
- As a user, token usage from these providers is priced into the budget cap (FR-5).

**Should have**
- A clear error when a model string names a provider with no key configured (which env
  var to set / which `forge auth` to run).
- Default pricing entries for the common new-provider models so cost tracking works out of
  the box.

**Could have**
- A `forge providers` command listing known providers, their env var, and key-present
  status.
- Per-provider `base_url` override in config (for self-hosted OpenAI-compatible gateways).

**Won't have (this iteration)**
- Provider-specific feature flags (extended thinking, prompt caching toggles, etc.).
- Automatic model discovery / live model lists.

### Non-goals
- This feature does **not** add any new auth *type* — it's API-key (BYOK) only, the same
  mechanism Anthropic/OpenAI already use. (Subscription auth is Part B.)
- This feature does **not** change the routing heuristics — only the set of reachable
  models.

## A3. Acceptance criteria

```
AC-A1  Given mesh.models.standard = "deepseek::deepseek-chat" and DEEPSEEK_API_KEY set
       When the standard tier is routed
       Then GenAiProvider calls the DeepSeek adapter with model "deepseek-chat"
       And the bearer key is the DeepSeek key (not Anthropic/OpenAI).

AC-A2  Given mesh.models.trivial = "openrouter::meta-llama/llama-3.3-70b-instruct"
        and OPENROUTER_API_KEY set
       When that tier is routed
       Then the request goes to base_url https://openrouter.ai/api/v1
       And carries Authorization: Bearer <openrouter key>.

AC-A3  Given a model string "gemini::gemini-2.5-flash"
       When the provider for that model is resolved
       Then the genai adapter selected is Gemini (not inferred-from-name guesswork).

AC-A4  Given `forge auth xai` is run and a key entered
       Then the key is stored under keyring service "forge", account "xai"
       And a later run with no XAI_API_KEY in env resolves the key from the keyring.

AC-A5 (negative)  Given mesh.models.complex = "gemini::gemini-2.5-pro" and NO gemini key
                   anywhere
                  When that tier is routed
                  Then the turn fails with a clear error naming GEMINI_API_KEY and
                       `forge auth gemini`
                  And no partial/garbage request is sent.

AC-A6  Given a priced response from any new provider
       When usage is recorded
       Then cost_usd is computed from that model's pricing (bundled default or override),
            falling back to 0.0 with a debug note if the model is unpriced — never a panic.
```

## A4. Impact analysis (Part A)

| Layer | File:line | Change |
|---|---|---|
| Provider resolution | `crates/forge-provider/src/genai_provider.rs:35` (`bare_model`) | Replace name-only inference with an explicit `provider::model` → `(AdapterKind, model, endpoint?)` resolver. |
| Client construction | `crates/forge-provider/src/genai_provider.rs:100` (`exec_chat_stream`) | Build the `genai::Client` with a `ServiceTargetResolver` so OpenRouter (and future custom-base providers) get `base_url` + bearer auth; native providers keep default targets. |
| Key resolution | `crates/forge-config/src/lib.rs:289` (`env_var_for`) | Add gemini/xai/deepseek/openrouter → `GEMINI_API_KEY` / `XAI_API_KEY` / `DEEPSEEK_API_KEY` / `OPENROUTER_API_KEY`. |
| Key injection | `crates/forge-config/src/lib.rs:327` (`inject_provider_keys`) | Iterate the full provider list, not the hard-coded `["anthropic","openai"]`. |
| Pricing | `crates/forge-mesh/src/pricing.rs` | Add default per-1k prices for common Gemini/xAI/DeepSeek models; OpenRouter is per-model (often unpriced → 0.0 fallback, documented). |
| CLI | `crates/forge-cli` `forge auth` | Accept the new provider names (it already takes a provider arg; just widen validation/help). |
| Permissions | `crates/forge-config/src/lib.rs:104` | **No change** — builtin safety rules are provider-agnostic. |

**Regression risk:** the `bare_model` → explicit-adapter change touches the hot path for
*every* provider including the existing three. Mitigation: keep the existing model strings
(`anthropic::…`, `openai::…`, `ollama::…`) working identically; cover with the existing
httpmock contract tests + new unit tests for the resolver mapping.

## A5. Technical design (Part A)

### Provider/adapter resolver

A pure function, unit-testable, no I/O:

```
provider::model  ─►  ProviderTarget {
                        adapter:  genai AdapterKind,     // Anthropic | OpenAI | Gemini | DeepSeek | Xai | Ollama ...
                        model:    String,                // bare model name passed to genai
                        endpoint: Option<&str>,          // Some("https://openrouter.ai/api/v1") for openrouter
                        key_provider: &str,               // which forge-config provider key to use ("openrouter", ...)
                      }
```

Mapping table (the source of truth — extend here to add a provider):

| prefix | genai adapter | endpoint | key (env var) |
|---|---|---|---|
| `anthropic::` | Anthropic | default | `ANTHROPIC_API_KEY` |
| `openai::` | OpenAI | default | `OPENAI_API_KEY` |
| `gemini::` | Gemini | default | `GEMINI_API_KEY` |
| `xai::` | Xai | default | `XAI_API_KEY` |
| `deepseek::` | DeepSeek | default | `DEEPSEEK_API_KEY` |
| `openrouter::` | OpenAI (compatible) | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `ollama::` | Ollama | default | (none) |
| *(no prefix)* | genai name-inference (back-compat) | default | best-effort |

### Client construction — genai native namespaces (implementation note)

**Simplified from the original plan after reading genai 0.6.5 source.** genai already ships
a namespace→adapter table (`adapter/adapter_kind.rs:207`) mapping `anthropic`, `openai`,
`gemini`, `xai`, `deepseek`, `ollama`, and `open_router` to their adapters, and each adapter
carries its own endpoint + default API-key env var (e.g. OpenRouter's adapter uses
`OPEN_ROUTER_API_KEY` and the OpenRouter base URL natively). So **no hand-rolled
`ServiceTargetResolver` is needed for the MVP**: we pass the `namespace::model` string
through to `exec_chat_stream` (genai splits on the first `::`), normalizing only Forge's
`openrouter` alias → genai's `open_router`. genai picks the adapter, endpoint, and auth.

This replaces the old `bare_model()` (which *stripped* the prefix and relied on fragile
name inference). `ServiceTargetResolver`/`AuthResolver` remain available (and the contract
tests still use `with_service_target_resolver_fn` to point genai at the httpmock loopback) —
a per-provider custom `base_url` override stays a Could-have for self-hosted gateways.

### Vertical slice (happy path)

```
config mesh.models.standard = "openrouter::deepseek/deepseek-chat"
        ↓ Router.route → RoutingDecision.model = "openrouter::deepseek/deepseek-chat"
        ↓ core resolves Provider for that model string
GenAiProvider.complete(model, …)
        ↓ resolve_target("openrouter::deepseek/deepseek-chat")
          → adapter=OpenAI-compat, model="deepseek/deepseek-chat",
            endpoint="https://openrouter.ai/api/v1", key=OPENROUTER_API_KEY
        ↓ client (ServiceTargetResolver applies endpoint+bearer) .exec_chat_stream
        ↓ stream chunks → on_text; End → usage
        ↓ mesh.pricing prices tokens → cost_usd
ModelResponse → recorded → budget aggregation (FR-5)
```

### Edge cases (Part A)

| Edge case | Behaviour |
|---|---|
| Prefix not in the table (e.g. `foobar::x`) | Fall back to genai name-inference; if that fails, `ProviderError::Request` with the unknown-prefix hint. |
| Provider needs key, none configured | Resolve fails *before* the network call with `ConfigError::MissingKey` naming the env var + `forge auth <p>` (AC-A5). |
| OpenRouter model name contains `::` (e.g. vendor routing) | `bare_model` semantics use `rsplit_once("::")` on the *first* `::` only — prefix is everything before the first `::`, model is the remainder, so `openrouter::deepseek/deepseek-chat` keeps the slash and any later colons. |
| Unpriced model | cost_usd = 0.0, debug log; never panic (AC-A6). |
| User sets `OPENAI_API_KEY` but routes `openrouter::…` | OpenRouter uses its own key only; the OpenAI key is never sent to OpenRouter. |

---

# PART B — CLI-bridge provider

## B1. Problem — JTBD

> When I already pay for Claude Pro/Max and/or ChatGPT, I want Forge to run turns on that
> subscription instead of a metered API key, so I'm not double-paying for model access —
> **without violating either vendor's ToS or risking my account.**

Who's affected: subscription holders (most individual devs). This is the feature the user
explicitly asked for after learning OAuth-token reuse is banned.

**The key idea.** The Feb-2026 Anthropic ban is specifically about using the *OAuth token*
in another product. It does **not** ban running the official `claude` binary. So Forge
never reads, stores, or transmits the token — it spawns the user's already-authenticated
official CLI as a subprocess and consumes its output. The credential stays inside the
sanctioned tool. Anthropic's 2026-06-15 note that programmatic `claude -p` usage draws from
a metered "Agent SDK credit" is evidence they *expect* headless subscription use.

### B-agy. Antigravity (`agy`) — free Gemini, third bridge

`CliKind::Antigravity` adds a third subscription bridge for Google's **Antigravity** CLI
(`agy`), exposing **free Gemini** (3.5 Flash / 3.1 Pro) plus Antigravity's proxied Claude/GPT.
Selected by the `agy-cli::` model prefix (e.g. `agy-cli::gemini-3.5-flash`,
`agy-cli::gemini-3.1-pro`, or bare `agy-cli::` for the CLI default).

Mechanics, mirroring the claude/codex bridges: Forge pipes the flattened transcript to
`agy -p --dangerously-skip-permissions [--model …]` over stdin and reads the printed answer.
Unlike claude/codex, `agy` has **no MCP/`--tools` wiring**, so it runs **text-mode only** (its
own agent with its own tools, `with_harness(false)` always) — it does not serve Forge's MCP
tool gate. Its `-p` output is **plain text** (not a JSON event stream), so `parse_antigravity_line`
treats each non-empty stdout line as answer text; there are no tool/usage events and usage is $0
(free tier).

Mesh integration is automatic via `CliKind::all()` + `provider_of`: `agy-cli` is discovered,
counted as a **subscription** ($0 marginal), priced at a ~1M-token Gemini window, and
rank-routed/failed-over exactly like the other bridges. Subscription tier (Free/Pro/Ultra) is set
in `forge init` (→ `[mesh.subscriptions] agy-cli = "<plan>"`). Setup: install Antigravity and run
`agy` once to log in. Verified end-to-end: `forge run --model agy-cli::gemini-3.5-flash` returns a
real Gemini answer at $0.

## B-ToS. Honest ToS analysis (read before shipping)

This is a **medium-low-confidence** compliance position and the spec says so plainly:

- **In Forge's favour:** Forge handles no credentials; it invokes a first-party binary the
  user installed and authenticated themselves; the user could run the exact same command by
  hand. The ban's text targets *token* use "in any other product."
- **Against:** Anthropic also prohibits "rout[ing] requests through Free, Pro, or Max plan
  credentials on behalf of their users." A reviewer could read "spawning `claude -p` from
  another tool" as exactly that. The clause is ambiguous about subprocess delegation.
- **OpenAI:** `codex exec` on a subscription is sanctioned for the user; whether a wrapper
  tool invoking it breaches "don't power third-party services" is likewise ambiguous.

**Decision:** ship it as **opt-in, off by default, clearly labelled**. Forge shows a
one-time notice on first CLI-bridge use:
> "CLI-bridge runs your locally-installed `{claude|codex}`. Forge never sees your login.
> Using subscription CLIs from third-party tools may be restricted by Anthropic/OpenAI
> terms — you run this at your own discretion. See docs/features/provider-integrations.md."

We do **not** claim it's compliant; we document the boundary and point at the primary ToS.
Recommend the user verify current Anthropic/OpenAI terms before relying on it.

## B2. Scope — user stories (MoSCoW)

**Must have**
- As a subscriber, I can set a tier to `claude-cli::sonnet` or `codex-cli::gpt-5-codex`
  and have Forge run that turn by spawning the official CLI.
- As a user, the assistant's text streams into Forge's TUI as the CLI produces it.
- As a user, CLI-bridge turns cost **$0** against my Forge USD budget cap (subscription
  billed, not API-metered).
- As a user, if the CLI is missing / not authenticated / too old, I get a clear,
  actionable error — not a hang or a panic.

**Should have**
- Token usage (if the CLI reports it in its JSON stream) is recorded for analytics, with
  cost_usd = 0.
- The one-time ToS notice on first use.

**Could have**
- `forge providers` shows CLI-bridge availability (binary found? version OK?).
- Configurable binary path / extra args per CLI bridge.

**Won't have (this iteration)**
- **Forge tool-calling *through* the CLI bridge.** The CLIs are full agents with their own
  tools; v1 runs them tool-disabled as a pure text backend (see B-tension). A turn routed
  to a CLI bridge cannot use Forge's tools.
- **Multi-turn CLI session reuse** (`--resume`/session ids). Each turn is a fresh
  invocation carrying the transcript in the prompt.
- Any handling of the OAuth token. Forge will never read `~/.claude/.credentials.json` or
  `~/.codex/auth.json`.

### Non-goals
- This feature does **not** make Forge a front-end for Claude Code / Codex agents — it uses
  them only as a model-completion backend.
- This feature does **not** bypass Forge's permission engine: because the CLI runs
  tool-disabled, the only side effect is reading the prompt Forge already assembled.

## B-tension. Resolving "the CLI is an agent, not an inference endpoint"

`claude -p "…"` and `codex exec "…"` run the vendor's *full agent loop* with its own
system prompt and its own tools. Forge already has a mesh + permission engine + tool loop.
Letting the CLI also run tools would mean two competing agent loops and bypass Forge's
safety. Resolution for v1:

- **Run the CLI tool-disabled** so it behaves as close to raw completion as possible:
  - Claude: `claude -p <prompt> --output-format stream-json --verbose --allowedTools ""`
    (no tools permitted) — optionally `--max-turns 1`.
  - Codex: `codex exec --json --sandbox read-only <prompt>` with no tool/network grants.
- **Forge owns the loop.** Forge assembles system+transcript into the single prompt, gets
  back assistant text, and runs *its own* tool-calling/permission/mesh logic around it.
- Consequence (documented non-goal): a CLI-bridge turn returns text only — it won't emit
  Forge-shaped `tool_calls`. The mesh should route tool-heavy work to API providers and use
  CLI-bridge tiers for chat/explanation/cheap bulk. This is acceptable for v1 and revisited
  if/when the CLIs expose a raw-completion mode.

## B3. Acceptance criteria

```
AC-B1  Given `claude` is installed and logged in, and mesh routes to "claude-cli::sonnet"
       When a turn runs
       Then Forge spawns `claude -p <prompt> --output-format stream-json --verbose
            --allowedTools ""`
       And streams the assistant text deltas to on_text as they arrive
       And returns the full assistant text in ModelResponse.content.

AC-B2  Given a CLI-bridge turn completes
       When usage is recorded
       Then cost_usd == 0.0 (subscription billed)
       And the turn does not advance the USD budget aggregation (FR-5).

AC-B3  Given `codex` is installed and logged in, and mesh routes to "codex-cli::gpt-5-codex"
       When a turn runs
       Then Forge spawns `codex exec --json --sandbox read-only <prompt>`
       And parses the JSONL events into streamed text + final content.

AC-B4 (negative — not installed)  Given the `claude` binary is not on PATH
       When a "claude-cli::…" turn runs
       Then the turn fails with ProviderError naming the missing binary and how to install
            /authenticate it
       And Forge does not hang.

AC-B5 (negative — not authenticated)  Given `claude` is installed but not logged in (it
        exits non-zero / emits an auth-error event)
       When a turn runs
       Then the error surfaced to the user says the CLI is not authenticated and to run its
            login command — Forge never tries to authenticate it.

AC-B6 (negative — timeout/hang)  Given the CLI produces no output for the configured
        timeout
       When the deadline passes
       Then the child process group is killed and a timeout error is returned (same kill
            discipline as the shell tool).

AC-B7  Given any CLI-bridge invocation
       When Forge builds the command
       Then Forge reads no credential files and sets no auth env vars — the only inputs are
            the prompt and non-secret flags.

AC-B8 (first-use notice)  Given the first CLI-bridge use in a session
       When the turn starts
       Then the one-time ToS/discretion notice is emitted once (Warning event).
```

## B4. Impact analysis (Part B)

| Layer | File:line | Change |
|---|---|---|
| Provider trait | `crates/forge-provider/src/lib.rs:49` | **No change** — `CliProvider` implements the existing `Provider` trait. |
| New provider | `crates/forge-provider/src/cli_provider.rs` (new) | `CliProvider` using `tokio::process::Command`, JSONL stream parsing, kill-on-timeout. |
| Provider selection | core's provider construction (where the model string picks a backend) | Route `claude-cli::*` / `codex-cli::*` strings to `CliProvider`; everything else to `GenAiProvider`. |
| Budget | FR-5 cost path | CLI-bridge `Usage.cost_usd == 0.0`; aggregation already sums `cost_usd`, so $0 turns are naturally free — verify they don't trip the cap. |
| Process safety | mirror `crates/forge-tools/src/shell.rs` | Reuse the process-group-kill + stdin=/dev/null discipline already proven there (unix `libc`, cfg-gated). |
| Config | `crates/forge-mesh` model strings | `claude-cli::` / `codex-cli::` are just model strings — no schema change. Optional `[providers.cli]` block is a Could-have. |

**Regression risk:** isolated — a new module behind the existing trait + a routing branch
on the model prefix. Existing providers untouched.

## B5. Technical design (Part B)

### CliProvider shape

```
pub struct CliProvider {
    kind: CliKind,            // ClaudeCode | Codex
    binary: String,           // "claude" | "codex" (overridable later)
    timeout: Duration,
    notice_shown: ...,        // one-time ToS notice latch (or handled in core)
}

impl Provider for CliProvider {
    async fn complete(&self, model, messages, _tools, on_text) -> Result<ModelResponse, ProviderError> {
        // 1. tools are IGNORED in v1 (tool-disabled) — documented non-goal.
        // 2. render system+transcript into a single prompt string.
        // 3. build argv (no secrets): kind-specific flags.
        // 4. spawn via tokio::process::Command, stdin=/dev/null, piped stdout/stderr,
        //    own process group; enforce self.timeout (kill group on deadline).
        // 5. read stdout line-by-line; parse each line as a JSON event:
        //       - assistant text delta  -> push to content + on_text(delta)
        //       - usage/result event    -> capture input/output tokens
        //       - error/auth event       -> map to ProviderError with actionable message
        // 6. on non-zero exit with no parsed content -> ProviderError (include stderr tail).
        // 7. Usage { input, output, cost_usd: 0.0 }.
    }
}
```

### Event parsing (per CLI)

- **Claude Code** (`--output-format stream-json --verbose`): NDJSON. First line is a
  `system`/`init` event (session id, model, tools). Assistant text arrives in
  `assistant` message events (and, with `--include-partial-messages`, token-level deltas).
  A terminal `result` event carries usage. Parse defensively: unknown event types are
  skipped; we key only on the fields we need, so CLI version drift degrades gracefully.
- **Codex** (`codex exec --json`): JSONL events to stdout, final agent message to stdout.
  Capture streamed text events; tolerate schema differences the same way.

Because both schemas can change across CLI versions, the parser is **field-tolerant**:
extract `type`/`role`/`text`/usage by best-effort lookup, never hard-fail on an unexpected
shape; if *zero* assistant text was parsed and exit code is 0, return the raw stdout as
content (last-resort) rather than an empty success.

### Negative-path handling

| Edge case | Behaviour | AC |
|---|---|---|
| Binary not on PATH | spawn error → `ProviderError::Request("claude not found — install Claude Code and run `claude` to log in")` | AC-B4 |
| Installed, not logged in | CLI exits non-zero / emits auth error → map to "not authenticated, run `claude`/`codex login`" | AC-B5 |
| CLI version too old (no `--output-format stream-json`) | spawn fails or emits usage/usage-flag error → error naming the minimum version | AC-B4 |
| Hang / no output | timeout → kill process group → timeout error | AC-B6 |
| Emits non-JSON noise on stdout | tolerated; non-JSON lines skipped; raw fallback if nothing parsed | — |
| Tools requested by Forge for this turn | ignored (v1 non-goal); doc'd. Mesh should not route tool-required turns here. | — |
| Huge output | stream incrementally; same truncation policy as elsewhere if buffering a tail for errors. | — |

### Security / boundary checks

- **AC-B7 is a test:** assert the constructed argv + env contain no credential paths and no
  `ANTHROPIC_*`/`OPENAI_*`/`*_API_KEY` auth vars; assert Forge never opens
  `~/.claude/.credentials.json` or `~/.codex/auth.json`.
- stdin set to `/dev/null` (no prompt-injection via inherited stdin); inherit a minimal env.
- The prompt is passed as an argv argument or via stdin pipe — never interpolated into a
  shell string (no `sh -c`), so there's no shell-injection surface.

---

## Definition of done (both parts)

- [ ] **A:** `provider::model` resolver maps all 7 prefixes to the right adapter/endpoint;
      unit-tested incl. `openrouter::vendor/model-with-colons`.
- [ ] **A:** `GenAiProvider` builds a client with `ServiceTargetResolver`; OpenRouter hits
      the custom base URL with bearer auth (httpmock contract test, no real key).
- [ ] **A:** `env_var_for` + `inject_provider_keys` + `forge auth` cover gemini/xai/
      deepseek/openrouter; missing-key error names the var + `forge auth` (AC-A5).
- [ ] **A:** pricing defaults added; unpriced model → 0.0, no panic (AC-A6).
- [ ] **B:** `CliProvider` spawns `claude`/`codex` with the documented tool-disabled flags,
      streams text, returns content + usage; cost_usd == 0.0 (AC-B1–B3).
- [ ] **B:** all negative paths return clear errors, never hang/panic (AC-B4–B6); timeout
      kills the process group.
- [ ] **B:** boundary test proves no credential file/env is read or set (AC-B7).
- [x] **B:** one-time ToS/discretion notice on first use (AC-B8) — emitted via `tracing::warn!`
      on the first CLI-bridge dispatch (`DispatchProvider`); surfacing it as a TUI
      `PresenterEvent::Warning` is a follow-up (the provider has no presenter handle). Docs
      carry the honest ToS analysis (§B-ToS).
- [x] **B verified live:** `tests/cli_bridge_live.rs` round-trips real `claude` (2.1.177) and
      `codex` (0.130.0) — text streamed, usage captured, cost $0, no tool calls. Gated behind
      `FORGE_CLI_BRIDGE_TESTS=1 -- --ignored` (needs an authenticated CLI; consumes quota).
- [ ] Existing provider contract tests + workspace tests stay green (no regression on the
      three current providers).
- [ ] `forge auth` help + README/docs list the new providers and the CLI-bridge opt-in.
- [ ] clippy `-D warnings` + fmt clean; CI green on Linux/macOS/Windows (CLI-bridge process
      code cfg-gated like the shell tool).

## Build order

1. **PR A — matrix:** resolver + ServiceTargetResolver + key/env/keyring + pricing + tests.
   (Self-contained, zero-risk, unlocks routing immediately.)
2. **PR B — CLI-bridge:** `CliProvider` + routing branch + negative paths + boundary test +
   ToS docs/notice. (Builds on A's routing seam; opt-in.)
