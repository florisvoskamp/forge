# Test Strategy: GenAiProvider (the real provider integration)

> Right-sized spec, in the same shape as a feature design but for a **test strategy**. It
> targets `crates/forge-provider` and the CI workflow. No Rust code is written here — this
> is the plan the implementation PR follows.

## 1. Problem (the untested headline feature + risk)

`GenAiProvider` (`crates/forge-provider/src/genai_provider.rs`) is the **real** Provider
implementation — it backs FR-3, "multi-provider abstraction with normalized streaming +
tool calls," for Anthropic / OpenAI / Ollama via the `genai` 0.6 crate. It does four
things that can silently break:

1. **Request mapping** — Forge `Message` / `ToolSpec` → genai `ChatRequest` (system/user/
   assistant/tool roles, assistant tool calls, replayed `ToolResponse`s, tool schemas).
   (`to_genai_messages`, `to_genai_tool`, lines 28–65.)
2. **Adapter inference** — `model.rsplit("::").next()` turns `ollama::llama3.2` into
   `llama3.2` so genai picks the adapter from the bare name (line 77).
3. **Streaming normalization** — drive `exec_chat_stream`, accumulate `Chunk` deltas, push
   each to the `TextSink`, and on `End` read `captured_usage`, `captured_first_text` (text-
   only-at-end providers), and `captured_into_tool_calls` (lines 96–135).
4. **Tool-call translation** — genai `ToolCall` → Forge `ToolCall { id, name, args }`
   (lines 122–131).

**Audit finding (confirmed):** every provider test in the suite runs against
`MockProvider` (`src/mock.rs`). `GenAiProvider` has **zero** automated tests. None of the
four behaviours above is exercised. A regression in role mapping, the `::` split, stream
accumulation, the empty-content fallback, usage capture, or tool-call translation would
ship green. MockProvider tests assert the *agent loop* contract, not the *genai mapping*
contract — they cannot catch this class of bug by construction.

**Constraint:** CI runs `cargo test --all --all-features` on ubuntu/macos/windows with **no
API keys** (`.github/workflows/ci.yml`). The strategy must exercise the real adapter logic
without spending tokens and without network egress in default CI.

## 2. Scope (MoSCoW)

**Must**
- **Layer 1 — Unit / translation tests.** Pure-function tests for request mapping, adapter
  inference, stream→`ModelResponse` reduction, and tool-call translation. No network.
- **Layer 2 — HTTP contract tests** against a local mock server (httpmock/wiremock),
  pointing genai at it via a base-URL override. Asserts the full path: request body shape,
  SSE streaming accumulation, and a tool-call round-trip. Runs in default CI.
- A **seam to inject a `genai::Client`** into `GenAiProvider` (required by Layer 2 — see §4).

**Should**
- **Layer 3 — Local integration test** against a real Ollama, gated off default CI (env-
  gated `#[ignore]`), verifying a real end-to-end turn + tool round-trip on the owner's
  machine.
- A multi-turn tool-loop contract test (assistant tool call → replayed `ToolResponse` →
  final text) at Layer 2.

**Could**
- An **opt-in scheduled CI job** that runs the integration layer against a containerized
  Ollama (GitHub Actions service container) — nightly, not per-PR.
- Snapshot/cassette (VCR-style) tests recorded from one real run and replayed offline.

**Won't (this iteration)**
- Live tests against Anthropic/OpenAI in CI (costs real tokens, needs secrets — out of
  scope; see §5 "honest limitations").
- Testing genai's own adapter internals (that's the dependency's responsibility; we test
  *our* mapping and the wire contract we depend on).

## 3. Acceptance criteria (Given / When / Then — tests that must exist and pass)

**AC-1 (Layer 1, request mapping).** *Given* a transcript with system + user + an assistant
message carrying a `ToolCall` + a `Tool`-role result, *when* it is mapped to genai messages,
*then* the output is system, user, assistant-text, assistant-tool-call, and a `ToolResponse`
whose id equals the original `tool_call_id`; an assistant message with empty content emits
**no** empty assistant text message.

**AC-2 (Layer 1, adapter inference).** *Given* models `ollama::llama3.2`, `openai::gpt-4o`,
and a bare `claude-3-5-sonnet`, *when* the model string is normalized, *then* the bare names
`llama3.2`, `gpt-4o`, `claude-3-5-sonnet` are produced (the `::` prefix is stripped; a name
without `::` is unchanged).

**AC-3 (Layer 1, tool schema).** *Given* a `ToolSpec` with name/description/JSON-schema,
*when* mapped, *then* a genai `Tool` carries the same name, description, and schema value.

**AC-4 (Layer 2, streaming).** *Given* a mock server returning a multi-chunk SSE text
response, *when* `complete` runs against a genai client pointed at it, *then* the `TextSink`
receives the deltas in order, `ModelResponse.content` equals their concatenation, and
`usage.input_tokens` / `output_tokens` reflect the response's usage block.

**AC-5 (Layer 2, end-only text fallback).** *Given* a mock response that carries the text
only in the terminal message (no incremental chunks), *when* `complete` runs, *then*
`content` is still populated (the `captured_first_text` branch, lines 116–121) and the sink
received it once.

**AC-6 (Layer 2, tool-call round-trip).** *Given* a mock response that emits a tool call,
*when* `complete` runs, *then* `ModelResponse.tool_calls` has one entry with the server's
`id`, `name`, and parsed `args`, and `wants_tools()` is true.

**AC-7 (Layer 2, error path).** *Given* the mock returns HTTP 500 / malformed SSE, *when*
`complete` runs, *then* it returns `Err(ProviderError::Request(_))` rather than panicking.

**AC-8 (Layer 3, gated integration).** *Given* a running Ollama and `FORGE_OLLAMA_TESTS=1`,
*when* the ignored test runs `complete("ollama::<model>", …)` with a tool, *then* a real
turn completes and (best-effort) a tool round-trip succeeds. *Given* the env var is unset,
*then* the test does not run in default `cargo test` and CI stays green with no Ollama.

**AC-9 (CI).** *Given* a PR with no secrets, *when* CI runs, *then* Layers 1 + 2 execute and
pass on all three OS targets; Layer 3 is skipped; the run uses no network egress to real
providers.

## 4. Impact analysis (files / deps / CI touched)

**Source — one small seam (required for Layer 2):**
- `crates/forge-provider/src/genai_provider.rs` — `GenAiProvider` currently holds a private
  `client: Client` built only via `Self::default()` (lines 17–26). There is **no way to
  inject a client**, so tests cannot point genai at a mock server. Add a
  `GenAiProvider::with_client(client: genai::Client) -> Self` (and keep `new()`/`Default`).
  This is the single production change; it is test-enabling and otherwise inert.
- Optionally extract the three pure mappers (`to_genai_messages`, `to_genai_tool`, and a new
  `model → bare name` helper) to be reachable from the test module (they are private module
  fns today — a `#[cfg(test)] mod tests` in the same file already has visibility, so no API
  change needed for Layer 1).

**Test modules:**
- `crates/forge-provider/src/genai_provider.rs` → add `#[cfg(test)] mod tests` for Layer 1
  (pure mappers, no async runtime needed beyond what exists).
- `crates/forge-provider/tests/genai_contract.rs` (new integration-test file) for Layer 2
  (spins up the mock server, builds an injected client, calls `complete`).
- `crates/forge-provider/tests/ollama_live.rs` (new) for Layer 3, all tests `#[ignore]` +
  env-gated.

**Dev-dependencies (`crates/forge-provider/Cargo.toml`, `[dev-dependencies]`):**
- `httpmock` (preferred: blocking-free, simple `Mock` matchers, returns a base URL) **or**
  `wiremock` (async, tower-based). Either works; pick `httpmock` for the lightest footprint.
- `tokio` (already present) — ensure `macros` + `rt-multi-thread` features for `#[tokio::test]`.
- No new **runtime** deps. `genai` 0.6 already exposes the override API (verified below).

**genai capability (verified against genai 0.6.5 source):**
- `Client::builder().with_service_target_resolver_fn(|tgt: ServiceTarget| { … })` lets us
  rewrite the resolved `ServiceTarget { endpoint, auth, model }`. We replace `endpoint` with
  `Endpoint::from_owned(mock_base_url)` and set `auth` to a dummy key
  (`AuthData::from_single("test")`), so genai sends real, correctly-shaped HTTP to the mock
  and needs no real key. (`src/client/builder.rs`, `src/resolver/endpoint.rs`,
  `src/client/service_target.rs`.) → **Layer 2 is feasible; no fallback needed.**
- If a future genai version removed this seam, the documented fallback is to test at Forge's
  `Provider` boundary with a hand-rolled stub `Provider` (loses wire-contract coverage but
  keeps mapping coverage). Not required today.

**CI (`.github/workflows/ci.yml`):**
- No structural change required for Layers 1 + 2 — they run under the existing
  `cargo test --all --all-features` matrix. Layer 3 is `#[ignore]`d so it is auto-skipped.
- Add (Could) a separate `integration` job, `schedule:`d (nightly) + `workflow_dispatch`,
  on `ubuntu-latest` only, with an Ollama **service container**, running
  `FORGE_OLLAMA_TESTS=1 cargo test -p forge-provider -- --ignored`. Kept off the PR path.

## 5. Technical design

### Layer 1 — Unit / translation tests (no network)
Target the pure functions directly in `genai_provider.rs`'s test module:
- **Messages:** build a `&[Message]` covering all four roles + assistant-with-tool-calls +
  empty-assistant-content, call `to_genai_messages`, and assert the resulting
  `Vec<ChatMessage>` length, ordering, role tags, and that the `ToolResponse` id round-trips
  `tool_call_id`. Assert the empty-content assistant produces no stray text message.
- **Adapter inference:** factor the `model.rsplit("::").next().unwrap_or(model)` logic into a
  `fn bare_model(model: &str) -> &str` and table-test it (`provider::model`, bare name,
  multi-`::`, empty).
- **Tool schema:** `to_genai_tool(spec)` → assert name/description; assert the schema value
  is carried through unchanged (compare `serde_json::Value`).
- These are synchronous and run in microseconds.

### Layer 2 — HTTP contract tests (mock server, real genai path)
Sketch (httpmock; OpenAI-compatible shape is simplest to assert):

```
// crates/forge-provider/tests/genai_contract.rs  (pseudocode)
let server = MockServer::start();                       // local 127.0.0.1:<port>

let _m = server.mock(|when, then| {
    when.method(POST).path("/v1/chat/completions");
    then.status(200)
        .header("content-type", "text/event-stream")
        .body(concat!(                                  // SSE chunks, OpenAI delta format
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}, ...}\n\n",
            "data: [DONE]\n\n",
        ));
});

let client = genai::Client::builder()
    .with_service_target_resolver_fn(move |mut t: ServiceTarget| {
        t.endpoint = Endpoint::from_owned(server.base_url());  // 127.0.0.1
        t.auth     = AuthData::from_single("test-key");         // dummy, never real
        Ok(t)
    })
    .build();

let provider = GenAiProvider::with_client(client);             // the new seam (§4)
let mut sink = String::new();
let res = provider.complete("openai::gpt-4o-mini", &msgs, &tools,
                            &mut |d| sink.push_str(d)).await?;

assert_eq!(sink, "Hello");
assert_eq!(res.content, "Hello");
assert_eq!(res.usage.input_tokens, 5);
assert_eq!(res.usage.output_tokens, 2);
```

Variants give us AC-5 (text only in the final SSE message → exercises `captured_first_text`),
AC-6 (a `tool_calls` delta → assert `res.tool_calls[0]` id/name/args, `wants_tools()`), and
AC-7 (return 500 / truncated SSE → assert `Err(ProviderError::Request(_))`). A multi-turn
test (Should) sends a transcript that already contains an assistant tool call + a `Tool`
result and asserts via the mock's *received request body* that the replayed `ToolResponse`
and the prior assistant tool call were serialized into the outgoing genai request — this is
the only layer that proves `to_genai_messages` round-trips over the wire.

**Why this avoids real APIs:** the resolver rewrites the endpoint to loopback and the auth
to a throwaway string before any byte leaves; genai builds and parses a *real* request/SSE
stream, so the adapter mapping is genuinely exercised, but the only server contacted is the
in-process mock. No keys, no egress.

### Layer 3 — Local integration (gated)
`crates/forge-provider/tests/ollama_live.rs`, every test annotated `#[ignore]` and guarded:
```
#[tokio::test]
#[ignore = "requires local Ollama; run with FORGE_OLLAMA_TESTS=1 -- --ignored"]
async fn ollama_round_trip() {
    if std::env::var("FORGE_OLLAMA_TESTS").is_err() { return; }
    let p = GenAiProvider::new();
    let res = p.complete("ollama::llama3.2", &[Message::user("say hi")], &[], &mut |_| {}).await.unwrap();
    assert!(!res.content.is_empty());
}
```
A second test advertises a trivial tool and asserts a tool call comes back (best-effort —
small local models are unreliable at tool use, so assert "either a tool call or non-empty
text," and document that the strict tool-loop assertion is the manual/owner check). Gating is
belt-and-suspenders: `#[ignore]` keeps it out of default `cargo test`; the env-var early
return keeps it green even if someone runs `--ignored` on a box with no Ollama.

### Gating mechanism summary
| Mechanism | Effect | Used by |
|---|---|---|
| `#[cfg(test)]` unit module | always runs | Layer 1 |
| `tests/*.rs` integration file | always runs in `cargo test` | Layer 2 |
| `#[ignore]` + `FORGE_OLLAMA_TESTS` env early-return | skipped unless explicitly opted in | Layer 3 |
| scheduled CI job + Ollama service container (Could) | nightly only, never per-PR | Layer 3 |

### Example test cases (layer / what it asserts / how it avoids real APIs)
| # | Layer | Asserts | Avoids real API by |
|---|---|---|---|
| 1 | 1 Unit | All four roles + assistant-tool-call + tool-result map to correct genai messages, ids round-trip, empty assistant content emits nothing | pure fn, no client/network |
| 2 | 1 Unit | `bare_model("ollama::llama3.2") == "llama3.2"`, bare/empty/multi-`::` cases | pure fn |
| 3 | 1 Unit | `to_genai_tool` carries name/description/schema unchanged | pure fn |
| 4 | 2 Contract | Multi-chunk SSE → ordered sink deltas, concatenated `content`, captured usage | endpoint rewritten to loopback mock, dummy auth |
| 5 | 2 Contract | Text only in terminal SSE message still fills `content` (`captured_first_text`) | mock server |
| 6 | 2 Contract | Tool-call delta → `ToolCall{id,name,args}`, `wants_tools()` true | mock server |
| 7 | 2 Contract | Outgoing request body contains replayed assistant tool call + `ToolResponse` (multi-turn) | mock inspects received body |
| 8 | 2 Contract | HTTP 500 / truncated SSE → `Err(ProviderError::Request)`, no panic | mock server |
| 9 | 3 Integration | Real Ollama turn returns non-empty content; tool advertised → call-or-text | gated `#[ignore]` + env var; runs only on owner box / nightly |

## 6. Definition of done
- The `with_client` seam exists on `GenAiProvider`; `new()`/`Default` behaviour unchanged.
- Layer 1 covers all three pure mappers (request, adapter inference, tool schema) — every
  match arm of `to_genai_messages` is hit.
- Layer 2 covers, at minimum, AC-4, AC-5, AC-6, AC-8(error) — i.e. streaming accumulation,
  end-only-text fallback, tool-call translation, and the error path — all against the mock
  server with no key. Multi-turn body assertion (AC-7) landed as Should.
- Layer 3 exists, is `#[ignore]`d + env-gated, and is documented in the crate's test README/
  comment with the exact run command.
- **Coverage target:** every behavioural branch in `complete` and the mappers
  (`Chunk` accumulation, `captured_usage`, `captured_first_text` fallback,
  `captured_into_tool_calls`, the `::` split, each role arm, the request error map) is
  exercised by Layer 1 or 2. Tool-call translation and streaming — the FR-3 guarantees — are
  no longer Mock-only.
- **CI green with no keys:** `cargo test --all --all-features` passes on ubuntu/macos/windows
  with Layers 1+2 running and Layer 3 skipped; no secrets referenced in `ci.yml`.

## 7. Honest limitations (what canNOT be tested without spending tokens)
- **Real Anthropic/OpenAI behaviour** (actual model output, real SSE framing quirks, real
  tool-call formatting, rate-limit/4xx/5xx semantics) is **not** covered in CI — the mock
  asserts the contract *we believe* each provider follows, not the provider's live reality.
  A provider changing its wire format would pass our mock and fail in production; mitigation
  is the optional recorded-cassette (Could) refreshed manually, plus the owner's local runs.
- **Ollama tool-calling reliability** is model-dependent; Layer 3 asserts capability, not
  determinism.
- We test *our* mapping against genai and against a faithful mock, not genai's own adapter
  correctness — that trust is delegated to the dependency and its version pin.
