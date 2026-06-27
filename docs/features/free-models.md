# Free cloud models in the Mesh

Forge routes any `provider::model` id through the [Model Mesh](../roadmap.md) cost-aware router
(FR-5): it picks the **cheapest usable** candidate per tier, and any model **not** listed in the
pricing table costs **$0** — so genuinely-free providers win automatically when their key is set,
and the mesh falls back down the candidate list otherwise.

These all work through Forge's `genai` backend; most are **native genai adapters** (just set the
key). Providers genai has no SDK adapter for (Cerebras, **NVIDIA NIM**, **SambaNova**, **Mistral**)
are wired via a custom OpenAI-compatible endpoint resolver — see
[Adding a provider](#adding-an-openai-compatible-provider).

## Providers & keys

| Forge namespace | Free? | API-key env (`forge auth <name>`) | Notes |
|---|---|---|---|
| `groq::` | free tier | `GROQ_API_KEY` | Fast. `llama-3.3-70b-versatile`, `llama-3.1-8b-instant`, `qwen3-32b` — tool-calling supported. |
| `gemini::` | free tier | `GEMINI_API_KEY` | Free tier = **Flash family** (`gemini-2.5-flash`, `gemini-3-flash`); Pro left the free tier in 2026. Tools supported. |
| `open_router::` | `:free` models | `OPEN_ROUTER_API_KEY` | Append `:free` to a model id (e.g. `open_router::deepseek/deepseek-r1:free`). Rate-limited. Tool support varies by model. |
| `opencode_go::` | free (OpenCode Zen) | `OPENCODE_GO_API_KEY` | OpenCode Zen's curated free coding models (designed for tool calling). |
| `github_copilot::` | free tier | `GITHUB_TOKEN` | GitHub Models inference gateway (`github_copilot::openai/gpt-4.1-mini`, …). |
| `mimo::` | free tier | `MIMO_API_KEY` | Xiaomi MiMo. |
| `minimax::` | free tier | `MINIMAX_API_KEY` | MiniMax. |
| `cerebras::` | free tier | `CEREBRAS_API_KEY` | Custom endpoint. `llama-3.3-70b`, `gpt-oss-120b`, `qwen-3-coder-480b` — very fast. |
| `nvidia::` | free dev tier | `NVIDIA_API_KEY` | **NVIDIA NIM** (`integrate.api.nvidia.com`). Seeds `deepseek-ai/deepseek-r1`, `meta/llama-3.1-405b-instruct`, `meta/llama-3.3-70b-instruct`, `qwen/qwen2.5-coder-32b-instruct`, `nvidia/llama-3.1-nemotron-70b-instruct`. ~40 RPM across 100+ models. |
| `sambanova::` | free tier | `SAMBANOVA_API_KEY` | Custom endpoint. `DeepSeek-V3.1`, `DeepSeek-R1`, `Meta-Llama-3.3-70B-Instruct`, `Llama-4-Maverick-17B-128E-Instruct`. |
| `mistral::` | free Experiment tier | `MISTRAL_API_KEY` | Custom endpoint. `mistral-large-latest`, `mistral-small-latest`, `codestral-latest`, `magistral-medium-latest`. |
| `cohere::` | free trial | `COHERE_API_KEY` | Native adapter. Command A (218B), Command R+. |

> **Model ids change over time** and free tiers shift month-to-month — treat the shipped defaults
> and the ids above as a starting point and edit `[mesh.models]` to taste. **Tool/function-calling
> support varies per free model**; route tool-heavy tiers to models documented to support tools
> (Groq llama-3.3-70b, Gemini Flash, OpenCode Zen coding models). The custom-endpoint providers
> (`nvidia`/`sambanova`/`mistral`/`cerebras`) are **listed live** via their OpenAI `/v1/models`
> endpoint when their key is set — the mesh sees the **full catalog** the key can reach (e.g. NIM
> surfaces 100+ models), not just the `seed_models` above. The seed ids are only a fallback when the
> live `/models` call fails (offline / endpoint down). Embedding & reranking ids are filtered out
> (they can't serve chat completions).

## Adding an OpenAI-compatible provider

Any provider exposing a standard `/chat/completions` endpoint is one row in
`CUSTOM_OPENAI_PROVIDERS` (`crates/forge-config/src/lib.rs`):

```rust
CustomProvider {
    namespace: "nvidia",
    endpoint: "https://integrate.api.nvidia.com/v1/",  // trailing slash
    env_var: "NVIDIA_API_KEY",
    free: true,
    label: "NVIDIA NIM — free developer tier (100+ models)",
    seed_models: &["deepseek-ai/deepseek-r1", "meta/llama-3.1-405b-instruct", /* … */],
},
```

That single row wires `forge auth nvidia`, env injection, mesh discovery, the free/paid flag,
cost-tier routing, and cross-provider failover — no genai SDK adapter needed. The resolver in
`forge-provider` retargets genai's OpenAI adapter at `endpoint` with the key from `env_var`, and
discovery lists the provider's models live from `{endpoint}models` (`list_custom_models`),
falling back to `seed_models` if that call fails. Slash-bearing ids
(`meta/llama-3.1-405b-instruct`) work: the `provider::model` split is on the first `::` only.

## Default tiers (shipped)

Each tier leads with a free candidate, then falls back:

```toml
[mesh.models]
trivial  = ["groq::llama-3.1-8b-instant", "ollama::llama3.2"]
standard = ["groq::llama-3.3-70b-versatile", "gemini::gemini-2.5-flash", "openai::gpt-4o-mini"]
complex  = ["groq::llama-3.3-70b-versatile", "claude-cli::", "anthropic::claude-opus-4-8"]
```

A free model with a configured key (cost $0) wins the cost-aware pick; with no key it's skipped and
the mesh routes to the next usable candidate (local `ollama::`, a subscription bridge, or a metered
API model). Set keys with `forge auth groq` (etc.) or the provider's env var.

## Example: an all-free setup

```toml
[mesh.models]
trivial  = ["groq::llama-3.1-8b-instant", "ollama::llama3.2"]
standard = ["opencode_go::deepseek-v4-flash", "groq::llama-3.3-70b-versatile"]
complex  = ["cerebras::llama-3.3-70b", "open_router::deepseek/deepseek-r1:free"]
```
