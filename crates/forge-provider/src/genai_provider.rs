//! `genai`-backed implementation of [`Provider`] (ADR-0003). genai 0.6 resolves an adapter
//! per `namespace::model` id, so this one backend covers Anthropic, OpenAI, Gemini, xAI,
//! DeepSeek, OpenRouter, Groq, OpenCode Zen (`opencode_go`), GitHub Models, MiMo, MiniMax,
//! Ollama, … plus Cerebras via a custom-endpoint resolver ([`build_client`]). Tool calling is
//! normalized: tools are advertised, the model's calls map back to Forge [`ToolCall`]s, and
//! prior tool results are replayed as genai tool responses so multi-step loops round-trip.

use async_trait::async_trait;
use forge_config::AzureProvider;
use forge_types::{EffortLevel, Message, Role, ToolCall, Usage};
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    Binary, CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent,
    ContentPart, MessageContent, ReasoningEffort, Tool, ToolCall as GenAiToolCall, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, Headers, ModelIden, ServiceTarget};

use crate::{
    CompletionOptions, EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec,
};

pub struct GenAiProvider {
    client: Client,
    pool: std::sync::Arc<KeyPool>,
    /// Per-completion output cap (`mesh.max_output_tokens`). `None` → no cap (provider default,
    /// often a model's full 65k max — too much for a free/low-credit account, see the 402 churn).
    max_output_tokens: Option<u32>,
}

impl Default for GenAiProvider {
    /// Route `Default` through [`GenAiProvider::new`] so the genai client is always built with our
    /// bundled-CA reqwest client. The macro-derived `Default` would instead build `genai`'s own
    /// default client, which calls `rustls-platform-verifier` and **panics** ("No CA certificates
    /// were loaded from the system") on a host without an OS trust store. Closing that landmine.
    fn default() -> Self {
        Self::new()
    }
}

impl GenAiProvider {
    pub fn new() -> Self {
        let pool = std::sync::Arc::new(KeyPool::from_config());
        Self {
            client: build_client_with(std::sync::Arc::clone(&pool)),
            pool,
            max_output_tokens: None,
        }
    }

    /// Construct with a caller-supplied `genai::Client`. Used by the HTTP contract tests to
    /// point genai at a local mock server; otherwise identical to [`GenAiProvider::new`].
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            pool: std::sync::Arc::new(KeyPool::default()),
            max_output_tokens: None,
        }
    }

    /// Cap the output tokens requested per completion. `0` disables the cap (provider default).
    pub fn with_max_output_tokens(mut self, cap: u32) -> Self {
        self.max_output_tokens = (cap > 0).then_some(cap);
        self
    }
}

/// Connect-phase timeout for the shared reqwest client (DNS + TCP + TLS only). See
/// [`build_reqwest_client`] — short because connecting should be fast, and it never truncates a long
/// streaming body (that's response-read time, which this deliberately does not bound).
const CONNECT_PHASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Hard ceiling on a single model-discovery network call (`/v1/models`, genai `all_model_names`).
/// The shared client has NO request-level timeout (it would cut off long streaming turns), so a hung
/// `/v1/models` load balancer would otherwise stall catalog refresh / startup indefinitely. Bound it
/// here instead — generous, well above any healthy listing response, so it's a backstop, not the
/// primary per-provider budget the discovery loop already applies.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Wrap a discovery future in [`DISCOVERY_TIMEOUT`], mapping an elapse to a retryable
/// [`ProviderError::Unavailable`] so a stalled `/v1/models` is skipped instead of hanging the caller.
/// Bounds the WHOLE future (connect + send + body read), which a client-level `.timeout()` can't do
/// without also truncating legitimate long streaming turns — hence this targeted wrapper.
async fn with_discovery_timeout<T>(
    what: &str,
    fut: impl std::future::Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    match tokio::time::timeout(DISCOVERY_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(ProviderError::Unavailable(format!(
            "{what} timed out after {}s",
            DISCOVERY_TIMEOUT.as_secs()
        ))),
    }
}

/// List the models a provider currently offers, as Forge `provider::model` ids, by querying the
/// provider's models endpoint via genai (`all_model_names`). Used by the auto-discovery mesh to
/// build a live catalog of usable models (docs/features/auto-discovery-mesh.md). The provider's
/// key + endpoint are resolved by genai from the environment. Providers genai can't list (no
/// native adapter, e.g. `cerebras`) return an error and are simply skipped by the caller.
pub async fn list_models(namespace: &str) -> Result<Vec<String>, ProviderError> {
    let kind = AdapterKind::from_lower_str(normalize_namespace(namespace))
        .ok_or_else(|| ProviderError::Request(format!("no genai adapter for `{namespace}`")))?;
    // Bounded so a hung listing endpoint can't stall startup even if a caller forgets its own budget.
    let names = with_discovery_timeout("model listing", async {
        Client::builder()
            .with_reqwest(build_reqwest_client())
            .build()
            .all_model_names(kind, None)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))
    })
    .await?;
    // Re-namespace with Forge's provider name (so `openrouter` stays `openrouter::…`).
    Ok(names
        .into_iter()
        .map(|n| format!("{namespace}::{n}"))
        .collect())
}

/// List a custom OpenAI-compatible provider's models **live** via its `/v1/models` endpoint (the
/// standard OpenAI models route). Works for ANY provider in `forge_config::CUSTOM_OPENAI_PROVIDERS`
/// — current or future — with no per-provider code: the endpoint and key env var come from the
/// registry row. genai has no SDK adapter for these providers (so [`list_models`] can't enumerate
/// them), but their OpenAI-compatible `models` endpoint returns the full catalog the key can reach,
/// so the mesh sees every model instead of a hand-seeded few. Returns `provider::id` ids; clearly
/// non-chat ids (embedding / reranking) are dropped — they can't serve chat completions and would
/// only add dead weight and failover churn to routing.
pub async fn list_custom_models(namespace: &str) -> Result<Vec<String>, ProviderError> {
    let cp = forge_config::custom_provider(namespace)
        .ok_or_else(|| ProviderError::Request(format!("`{namespace}` is not a custom provider")))?;
    // A keyless local server (LM Studio / llama.cpp / vLLM with no auth) has no key — send a
    // placeholder bearer it ignores, so its `/v1/models` is still enumerable. Keyed providers use
    // their real key (env or keyring).
    let key = match forge_config::api_key(namespace) {
        Ok(k) if !k.is_empty() => k,
        _ => LOCAL_PLACEHOLDER_KEY.to_string(),
    };
    let url = format!("{}models", cp.endpoint);
    // Bound the whole call so a hung load balancer can't stall catalog refresh / startup.
    with_discovery_timeout(&format!("{namespace} `/models` listing"), async move {
        let resp = build_reqwest_client()
            .get(&url)
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ProviderError::Request(format!(
                "{namespace} `/models` returned HTTP {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        let data = body.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
            ProviderError::Request(format!("{namespace} `/models`: no `data` array"))
        })?;
        Ok(data
            .iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
            .map(|id| format!("{namespace}::{id}"))
            .filter(|id| !forge_config::is_non_chat_model(id))
            .collect())
    })
    .await
}

/// Whether `namespace` has a genai adapter that can LIST its models (i.e. [`list_models`] can work).
/// Some providers are completion-only: Cerebras has no native adapter and is reached via the
/// custom service-target resolver in [`build_client`], so it answers completions fine but cannot be
/// enumerated. The caller uses this to skip such providers in auto-discovery WITHOUT logging a
/// scary "discovery failed — check your key" warning (the key is fine; they're just config-only).
pub fn is_discoverable(namespace: &str) -> bool {
    // Enterprise gateways have a genai adapter (so they route) but no usable model-LISTING endpoint
    // for our flow — Bedrock/Vertex want pinned deployment ids and per-account model access, and
    // genai can't enumerate them. Treat them like Cerebras: completion-only, skipped quietly in
    // discovery (users pin `bedrock::…`/`vertex::…` ids) instead of logging a false "check your key".
    if NON_LISTABLE_NATIVE.contains(&namespace) {
        return false;
    }
    AdapterKind::from_lower_str(normalize_namespace(namespace)).is_some()
}

/// Native providers genai can ROUTE to but not LIST (no enumerable models endpoint for our flow).
const NON_LISTABLE_NATIVE: &[&str] = &["bedrock", "vertex"];

/// Bearer token sent to keyless local OpenAI-compatible servers (LM Studio / llama.cpp / vLLM that
/// require no auth). They ignore it; genai's OpenAI adapter just needs *some* credential present.
const LOCAL_PLACEHOLDER_KEY: &str = "forge-local";

/// Build a `reqwest::Client` with Mozilla's bundled root CAs (`webpki-root-certs`) as the sole
/// trust store. This makes HTTPS independent of the OS certificate store so it works even on bare
/// containers that have no `ca-certificates` package installed.
/// A general-purpose `reqwest::Client` that trusts Mozilla's bundled root CAs, so outbound HTTPS
/// works even on bare containers without the `ca-certificates` package. Use this instead of
/// `reqwest::Client::new()` anywhere in Forge — `Client::new()` builds with the OS trust store and
/// **panics** internally on a system that has none.
pub fn bundled_http_client() -> reqwest::Client {
    build_reqwest_client()
}

fn build_reqwest_client() -> reqwest::Client {
    let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok());
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .gzip(true)
        .pool_max_idle_per_host(4)
        // Connect-only timeout: bounds DNS + TCP + TLS so a dead/hung load balancer can't wedge a
        // request at the connect phase forever. This is SAFE for long streaming turns — it caps only
        // connection establishment, NOT the response body read, so a 10-minute generation is never
        // truncated. (A whole-request `.timeout()` WOULD truncate streams, so we deliberately don't
        // set one here; discovery calls are bounded separately by `with_discovery_timeout`.)
        .connect_timeout(CONNECT_PHASE_TIMEOUT)
        .http2_keep_alive_interval(Some(std::time::Duration::from_secs(20)))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .http2_adaptive_window(true)
        .tls_certs_only(certs)
        .build()
        .expect("failed to build reqwest client with bundled CA certificates")
}

/// Build the genai client with a custom-endpoint resolver for OpenAI-compatible providers genai has
/// no native adapter for — every entry in `forge_config::CUSTOM_OPENAI_PROVIDERS` (Cerebras, NVIDIA
/// NIM, SambaNova, Mistral, …). genai keeps the full `provider::…` string as the model name (unknown
/// namespace → Ollama fallback), so the resolver detects the namespace, strips it, and retargets the
/// OpenAI adapter at the registered endpoint + key. All native namespaces
/// (groq/gemini/cohere/open_router/opencode_go/github_copilot/mimo/minimax/…) pass through unchanged.
/// Round-robin pool of API keys per provider, snapshotted from config at client-build time. It
/// powers multi-key rotation: with several keys for one provider, requests round-robin across them
/// to multiply a free tier's per-key rate limit and to fail over within the provider on a 429 (the
/// retry lands on the next key). Rotation engages ONLY for providers with ≥2 keys — with a single
/// key [`KeyPool::next`] returns `None` and the genai env-resolved default is used unchanged, so
/// single-key (and paid, cache-sensitive) providers are unaffected.
#[derive(Default)]
pub(crate) struct KeyPool {
    providers: std::collections::HashMap<String, (Vec<String>, std::sync::atomic::AtomicUsize)>,
}

impl KeyPool {
    /// Snapshot every keyed provider that has ≥2 configured keys.
    fn from_config() -> Self {
        let mut providers = std::collections::HashMap::new();
        for p in forge_config::known_key_providers() {
            let keys = forge_config::api_keys(p);
            if keys.len() >= 2 {
                providers.insert(
                    p.to_string(),
                    (keys, std::sync::atomic::AtomicUsize::new(0)),
                );
            }
        }
        Self { providers }
    }

    /// The next key for `provider` (round-robin), or `None` when it has <2 keys (no rotation).
    fn next(&self, provider: &str) -> Option<String> {
        let (keys, cursor) = self.providers.get(provider)?;
        let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % keys.len();
        Some(keys[i].clone())
    }

    /// Whether `provider` has ≥2 keys and therefore supports intra-provider key rotation.
    pub(crate) fn has_rotation(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }
}

pub(crate) fn build_client() -> Client {
    build_client_with(std::sync::Arc::new(KeyPool::from_config()))
}

/// Build the genai client with a key-rotation `pool` captured by the service-target resolver. Azure
/// config is read from the loaded config (`[providers.azure]`).
pub(crate) fn build_client_with(pool: std::sync::Arc<KeyPool>) -> Client {
    build_client_full(pool, forge_config::azure_provider().cloned())
}

/// Build the Azure OpenAI [`ServiceTarget`]: retarget genai's OpenAI adapter (so the request BODY is
/// standard OpenAI chat-completions — tool calls included) at Azure's deployment-scoped URL with an
/// `api-key` header, via a per-request `AuthData::RequestOverride`. genai's `exec_chat`/`exec_chat_stream`
/// replace the adapter's URL+headers with these, so `api-version` (query) and `api-key` (header) —
/// neither expressible through the standard OpenAI resolver — are honored. The deployment name is the
/// body `model`; Azure routes by the URL deployment regardless.
pub(crate) fn azure_service_target(
    azure: &AzureProvider,
    deployment: &str,
    key: &str,
) -> ServiceTarget {
    let url = azure.chat_completions_url(deployment);
    ServiceTarget {
        // Unused once `RequestOverride` replaces the URL, but `ServiceTarget` requires an endpoint.
        endpoint: Endpoint::from_owned(url.clone()),
        auth: AuthData::RequestOverride {
            url,
            headers: Headers::from(vec![("api-key".to_string(), key.to_string())]),
        },
        model: ModelIden::new(AdapterKind::OpenAI, deployment.to_string()),
    }
}

/// Build the genai client with the key-rotation `pool` and an optional resolved Azure provider,
/// both captured by the service-target resolver. Split from [`build_client_with`] so tests can inject
/// an Azure provider pointing at a mock HTTP server without touching global config.
pub(crate) fn build_client_full(
    pool: std::sync::Arc<KeyPool>,
    azure: Option<AzureProvider>,
) -> Client {
    let resolver = ServiceTargetResolver::from_resolver_fn(
        move |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            // Azure OpenAI (`azure::<deployment>`): genai has no Azure adapter and the deployment URL +
            // `api-version` query + `api-key` header don't fit the standard OpenAI resolver, so build a
            // per-request override. Key precedence mirrors the custom branch: rotated pool key > env var.
            if let Some(azure) = azure.as_ref() {
                if st.model.model_name.namespace_is(forge_config::AZURE_NS) {
                    let deployment = st.model.model_name.namespace_and_name().1.to_string();
                    let key = pool
                        .next(forge_config::AZURE_NS)
                        .or_else(|| std::env::var(&azure.env_var).ok().filter(|k| !k.is_empty()))
                        .unwrap_or_default();
                    return Ok(azure_service_target(azure, &deployment, &key));
                }
            }
            // Custom OpenAI-compatible providers (no native genai adapter): genai keeps the full
            // `provider::model` string as the model name (unknown namespace → Ollama fallback), so
            // detect the namespace, strip it, and retarget the OpenAI adapter at the registered
            // endpoint + key. One match drives Cerebras, NVIDIA NIM, SambaNova, Mistral, … — adding
            // a provider is a row in `forge_config::CUSTOM_OPENAI_PROVIDERS`, no code change here.
            // A rotated key (≥2 configured) is substituted; otherwise the env default is used.
            for cp in forge_config::custom_providers() {
                if st.model.model_name.namespace_is(cp.namespace) {
                    let bare = st.model.model_name.namespace_and_name().1.to_string();
                    // Auth precedence: a rotated key (≥2 configured) > the provider's env var when
                    // set (genai reads it — fast, no keyring in the hot path; keyring keys are copied
                    // to env by `inject_provider_keys` at startup) > a placeholder for keyless local
                    // servers (LM Studio / llama.cpp / vLLM) so they still route. `env_var` is empty
                    // for runtime-registered keyless providers.
                    let auth = if let Some(key) = pool.next(cp.namespace) {
                        AuthData::from_single(key)
                    } else if !cp.env_var.is_empty()
                        && std::env::var(cp.env_var)
                            .map(|v| !v.is_empty())
                            .unwrap_or(false)
                    {
                        AuthData::from_env(cp.env_var)
                    } else {
                        AuthData::from_single(LOCAL_PLACEHOLDER_KEY)
                    };
                    return Ok(ServiceTarget {
                        endpoint: Endpoint::from_owned(cp.endpoint.to_string()),
                        auth,
                        model: ModelIden::new(AdapterKind::OpenAI, bare),
                    });
                }
            }
            // Route `ollama::` through ollama's OpenAI-compatible endpoint instead of genai's
            // native Ollama adapter. The native path leaves tool calls from models that emit
            // Hermes/Qwen-style `<tool_call>…</tool_call>` XML (e.g. qwen3-coder) unparsed — they
            // leak into message text and the turn dead-ends with "empty response". Ollama's `/v1`
            // server parses those into structured tool_calls, so the OpenAI adapter drives them
            // correctly. ollama ignores the bearer token; a placeholder satisfies the adapter.
            // genai recognises `ollama` as a native adapter and strips the namespace, so match on
            // the resolved adapter kind (not the namespace, which is gone by here).
            if st.model.adapter_kind == AdapterKind::Ollama {
                let bare = st.model.model_name.namespace_and_name().1.to_string();
                let host = std::env::var("OLLAMA_HOST")
                    .unwrap_or_else(|_| "http://localhost:11434".into());
                return Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(ollama_v1_endpoint(&host)),
                    auth: AuthData::from_single("ollama"),
                    model: ModelIden::new(AdapterKind::OpenAI, bare),
                });
            }
            // Native-adapter providers (groq/gemini/openai/…): genai has already set
            // `auth = FromEnv(<default var>)`. Recover the Forge provider from that env-var name and,
            // if it has ≥2 keys, substitute the next rotated key. Single-key providers fall through
            // unchanged (cache locality preserved).
            let rotated = match &st.auth {
                AuthData::FromEnv(var) => {
                    forge_config::provider_for_env_var(var).and_then(|p| pool.next(p))
                }
                _ => None,
            };
            if let Some(key) = rotated {
                return Ok(ServiceTarget {
                    auth: AuthData::from_single(key),
                    ..st
                });
            }
            Ok(st)
        },
    );
    Client::builder()
        .with_reqwest(build_reqwest_client())
        .with_service_target_resolver(resolver)
        .build()
}

/// Build ollama's OpenAI-compatible base URL from an `OLLAMA_HOST` value. The host is often bare
/// (`127.0.0.1:11434`) or lacks the trailing `/v1/`; genai needs a full, scheme-qualified URL.
fn ollama_v1_endpoint(host: &str) -> String {
    let host = if host.starts_with("http") {
        host.to_string()
    } else {
        format!("http://{host}")
    };
    format!("{}/v1/", host.trim_end_matches('/'))
}

/// Forge model ids are `"provider::model"`. genai 0.6 resolves the adapter from a
/// `namespace::name` prefix directly (its namespace table covers anthropic, openai, gemini,
/// xai, deepseek, ollama, open_router, …), so we pass the namespaced form straight through
/// rather than stripping it — that selects the right adapter (and its endpoint + default
/// API-key env var) explicitly instead of relying on name inference. The only fix-up is
/// Forge's `openrouter` alias → genai's `open_router` namespace. A model with no `::` is
/// passed verbatim (genai falls back to name inference).
pub(crate) fn to_genai_model(model: &str) -> String {
    match model.split_once("::") {
        Some((prefix, name)) => format!("{}::{}", normalize_namespace(prefix), name),
        None => model.to_string(),
    }
}

/// Map a Forge provider prefix to the namespace genai expects. Identity for everything except
/// `openrouter` → genai's `open_router`, and `bedrock` → genai's `bedrock_api` (the Bearer-token
/// AWS Bedrock Converse adapter; the SigV4 variant is `bedrock_sigv4`, not wired here). `vertex`
/// matches genai's namespace as-is.
fn normalize_namespace(prefix: &str) -> &str {
    match prefix {
        "openrouter" => "open_router",
        "bedrock" => "bedrock_api",
        other => other,
    }
}

fn to_genai_messages(messages: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => out.push(ChatMessage::system(m.content.clone())),
            Role::User if m.images.is_empty() => out.push(ChatMessage::user(m.content.clone())),
            Role::User => {
                // Multimodal user turn: text part (if any) followed by each image as a binary part.
                let mut parts: Vec<ContentPart> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(ContentPart::from_text(m.content.clone()));
                }
                for img in &m.images {
                    parts.push(ContentPart::Binary(Binary::from_base64(
                        img.media_type.clone(),
                        img.data_base64.clone(),
                        None,
                    )));
                }
                out.push(ChatMessage::user(MessageContent::from_parts(parts)));
            }
            Role::Assistant => {
                if !m.content.is_empty() {
                    out.push(ChatMessage::assistant(m.content.clone()));
                }
                if !m.tool_calls.is_empty() {
                    let calls: Vec<GenAiToolCall> = m
                        .tool_calls
                        .iter()
                        .map(|c| GenAiToolCall {
                            call_id: c.id.clone(),
                            fn_name: c.name.clone(),
                            fn_arguments: c.args.clone(),
                            thought_signatures: None,
                        })
                        .collect();
                    out.push(ChatMessage::from(calls));
                }
            }
            Role::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                out.push(ChatMessage::from(ToolResponse::new(id, m.content.clone())));
            }
        }
    }
    out
}

/// Mark Anthropic/OpenAI prompt-cache breakpoints on the stable prefix of the transcript: the
/// system message (system prompt + persona) and the final message (the whole conversation up to
/// this turn). On the next turn that prefix is read from cache instead of re-billed at full input
/// price — the single biggest cost lever for a long agent loop. Providers without prompt caching
/// (and sub-threshold prefixes, e.g. Anthropic's 1024-token minimum) silently ignore the hint, so
/// this is always safe to set.
fn mark_cache_breakpoints(msgs: &mut [ChatMessage]) {
    if msgs.is_empty() {
        return;
    }
    // Anchor on the END of the leading system run, not just the first system message: Forge emits
    // several stacked system messages (base prompt + env + AGENTS.md + skill guidance), and a
    // breakpoint caches everything UP TO it — so marking the last leading system message caches the
    // whole standing prefix instead of re-billing all but the first every turn. Plus a breakpoint on
    // the final message so the rest of the conversation prefix is cached for the next turn's reuse.
    let last_leading_system = msgs
        .iter()
        .take_while(|m| m.role == ChatRole::System)
        .count()
        .checked_sub(1);
    let last = msgs.len() - 1;
    for idx in [last_leading_system, Some(last)].into_iter().flatten() {
        msgs[idx].options = Some(CacheControl::Ephemeral.into());
    }
}

fn to_genai_tool(spec: &ToolSpec) -> Tool {
    Tool::new(spec.name.clone())
        .with_description(spec.description.clone())
        .with_schema(spec.schema.clone())
}

/// How long to wait for the model to start responding before treating it as down.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a started stream may go silent before we treat it as stalled. This is per-chunk:
/// a long generation keeps resetting it, so only a genuinely hung stream trips it.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Build the retryable error for a connect/stream stall (so the mesh fails over).
fn stall_error(what: &str, after: std::time::Duration) -> ProviderError {
    ProviderError::Unavailable(format!("{what} (no data for {}s)", after.as_secs()))
}

/// Collapse a provider error to a short, single-line message — providers (esp. Gemini) return
/// a multi-line JSON body that would otherwise flood the TUI / logs. Keeps the first line, caps
/// length, strips the noisy `Body: {…}` tail.
fn short(s: &str) -> String {
    let head = s.split("\nBody:").next().unwrap_or(s);
    let line = head.lines().next().unwrap_or(head).trim();
    let line = line
        .strip_prefix("Web stream error for model ")
        .unwrap_or(line);
    if line.chars().count() > 160 {
        let cut: String = line.chars().take(157).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// Map a `genai::Error` to a classified [`ProviderError`] so the mesh can decide whether to
/// bench the model + fail over (429 / 5xx / auth) or fail the turn (everything else). Uses the
/// typed `StatusCode`/`HeaderMap` where genai exposes them (`HttpError`, `WebModelCall`); for
/// the *streaming* path Forge uses, genai only carries a string, so we scan it. Messages are
/// shortened (`short`) so a multi-line JSON body never reaches the UI.
fn classify_genai_error(err: &genai::Error) -> ProviderError {
    use genai::webc::Error as WebcError;
    match err {
        genai::Error::HttpError { status, body, .. } => classify_status(
            status.as_u16(),
            err.to_string(),
            body,
            parse_retry_after_body(body),
        ),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            WebcError::ResponseFailedStatus {
                status,
                body,
                headers,
            } => {
                // `Retry-After` header (delta-seconds), else the body's `retryDelay`.
                let retry_after = headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| {
                        let t = v.trim();
                        t.parse::<u64>()
                            .ok()
                            .map(std::time::Duration::from_secs)
                            .or_else(|| parse_secs(t).and_then(duration_from_secs))
                    })
                    .or_else(|| parse_retry_after_body(body));
                classify_status(status.as_u16(), err.to_string(), body, retry_after)
            }
            other => ProviderError::Unavailable(short(&other.to_string())),
        },
        // Streaming path: genai gives no typed HTTP status, only a string. Prefer a STRUCTURED read
        // when the cause is (or embeds) a JSON error body — classify on `error.code`/`error.status`/
        // `error.type` instead of substring-guessing — and fall back to text scanning otherwise.
        genai::Error::WebStream { cause, .. } => parse_embedded_json(cause)
            .as_ref()
            .and_then(classify_error_body)
            .unwrap_or_else(|| classify_text(cause, err.to_string())),
        // In-stream error event with a STRUCTURED JSON body — classify on its typed fields first.
        genai::Error::ChatResponse { body, .. } => classify_error_body(body)
            .unwrap_or_else(|| classify_text(&body.to_string(), err.to_string())),
        // A bad/truncated stream chunk — transient, worth trying elsewhere.
        genai::Error::StreamParse { .. } => ProviderError::Unavailable(short(&err.to_string())),
        other => {
            let s = other.to_string();
            // A genai "Resolver error" (adapter/auth couldn't be built — almost always a missing
            // API key) is PERMANENT for this turn: retrying dispatches the same keyless model and
            // fails identically. Class it as Auth so the mesh EXCLUDES it (long bench + periodic
            // re-probe) instead of surfacing the raw "Resolver error for model 'groq::…'" and, on
            // the last-resort path, re-benching it forever.
            if is_auth_config_failure(&s) {
                ProviderError::Auth(short(&s))
            } else {
                ProviderError::Request(short(&s))
            }
        }
    }
}

/// Classify a provider error from its STRUCTURED JSON body — the typed signal genai exposes on the
/// `ChatResponse` stream-error path (and that some providers embed in a `WebStream` cause) — instead
/// of substring-matching the stringified form. Reads the shapes real providers actually emit:
///   - a numeric HTTP-ish `error.code` (OpenAI/Gemini `429`) → reuse [`classify_status`] (most
///     reliable: the same code-based path the typed HTTP errors take);
///   - Google's `error.status` enum (`RESOURCE_EXHAUSTED` / `UNAUTHENTICATED` / `UNAVAILABLE` / …);
///   - OpenAI/Anthropic string `error.code` / `error.type` (`rate_limit_exceeded`,
///     `insufficient_quota`, `rate_limit_error`, `overloaded_error`, `authentication_error`, …).
///
/// Returns `None` when the body isn't one of these shapes, so the caller falls back to text scanning.
/// A provider tweaking its prose no longer silently breaks classification — the typed field still
/// carries the signal, and the per-provider contract tests assert each shape.
fn classify_error_body(body: &serde_json::Value) -> Option<ProviderError> {
    let err = body.get("error").unwrap_or(body);
    let raw = body.to_string();
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| raw.clone());
    // Permanent incapability / payment markers win first (mirrors `classify_status`' ordering), so a
    // "requires more credits" / "function calling not supported" body is EXCLUDED, not retried.
    if is_capability_failure(&raw) {
        return Some(ProviderError::Capability(short(&msg)));
    }
    // 1. Numeric HTTP code — delegate to the shared status classifier (handles 402/429/401/5xx/…).
    if let Some(code) = err.get("code").and_then(json_status_code) {
        return Some(classify_status(
            code,
            msg,
            &raw,
            parse_retry_after_body(&raw),
        ));
    }
    let m = short(&msg);
    // 2. Google RPC status enum.
    if let Some(status) = err.get("status").and_then(|s| s.as_str()) {
        match status {
            "RESOURCE_EXHAUSTED" => {
                return Some(ProviderError::RateLimited {
                    message: m,
                    retry_after: parse_retry_after_body(&raw).filter(|_| !quota_is_exhausted(&raw)),
                })
            }
            "UNAUTHENTICATED" | "PERMISSION_DENIED" => return Some(ProviderError::Auth(m)),
            "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED" => {
                return Some(ProviderError::Unavailable(m))
            }
            _ => {}
        }
    }
    // 3. OpenAI/Anthropic string code or type.
    let code_type = err
        .get("code")
        .and_then(|c| c.as_str())
        .or_else(|| err.get("type").and_then(|t| t.as_str()));
    if let Some(ct) = code_type {
        let l = ct.to_lowercase();
        if l.contains("rate_limit") || l.contains("resource_exhausted") || l.contains("overloaded")
        {
            return Some(ProviderError::RateLimited {
                message: m,
                retry_after: parse_retry_after_body(&raw),
            });
        }
        if l.contains("insufficient_quota") || l.contains("billing") || l.contains("payment") {
            return Some(ProviderError::Capability(m));
        }
        if l.contains("authentication")
            || l.contains("invalid_api_key")
            || l.contains("unauthorized")
            || l.contains("permission")
        {
            return Some(ProviderError::Auth(m));
        }
    }
    None
}

/// Read a JSON value as an HTTP status code: an integer (`429`) or a numeric string (`"429"`),
/// bounded to a plausible 1xx–5xx range so a random `code: 0` / `code: 20000` isn't misread.
fn json_status_code(v: &serde_json::Value) -> Option<u16> {
    v.as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u16>().ok()))
        .filter(|c| (100..=599).contains(c))
}

/// Best-effort extract a JSON error body from a free-text stream `cause`: the cause is the whole
/// JSON, or has one embedded (`...Body: {…}`). Returns the parsed value when found.
fn parse_embedded_json(cause: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(cause.trim()) {
        return Some(v);
    }
    let start = cause.find('{')?;
    let end = cause.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&cause[start..=end]).ok()
}

/// Markers of a no-credentials / misconfigured-provider failure (genai's resolver couldn't build
/// the adapter, or the provider rejected an absent key). Treated as [`ProviderError::Auth`] —
/// permanent for the session, so the model is excluded rather than benched-and-retried.
fn is_auth_config_failure(text: &str) -> bool {
    let l = text.to_lowercase();
    l.contains("resolver error")
        || l.contains("no auth")
        || l.contains("missing api key")
        || l.contains("no api key")
        || l.contains("api key not")
        || l.contains("requires an api key")
        || l.contains("requires api key")
}

/// A 429 whose quota is per-day or flat-out zero (a free-tier model that's disabled, like
/// Gemini's `limit: 0`). The server still hands back a tiny `retryDelay` (e.g. 7s), but retrying
/// in 7s just fails again and thrashes — so we drop that hint and let the longer default bench
/// apply. Genuine per-minute limits (no such marker) keep their short delay.
fn quota_is_exhausted(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("limit: 0") || l.contains("perday") || l.contains("per day") || l.contains("per-day")
}

/// Markers of a PERMANENT, model-specific incapability — this model can never serve Forge's
/// tool-using turns, or the account can't afford it. These errors recur identically on every
/// call, so the model is *excluded* rather than benched-and-retried (the source of the
/// "every model is failing" churn). Checked against the raw error body, which carries the
/// provider's real message even when the HTTP status is generic (400/404).
fn is_capability_failure(text: &str) -> bool {
    let l = text.to_lowercase();
    // Standalone markers that unambiguously mean "this model can't serve us".
    const MARKERS: &[&str] = &[
        // OpenRouter: no provider endpoint exposes tool use for this model.
        "no endpoints found that support tool use",
        // OpenRouter / generic: feature explicitly unsupported.
        "does not support feature: function-calling",
        // MiniMax (via opencode_go): rejects our tool payload outright.
        "function name or parameters is empty",
        // Account can't afford the request (OpenRouter 402 free tier).
        "requires more credits",
        "can only afford",
        "insufficient credit",
        "insufficient_quota",
        // A 402 surfaced via the STREAMING path (no typed HTTP status) — e.g. SambaNova:
        // "A payment method is required to use `<model>`". Permanent for this key, so EXCLUDE the
        // model rather than benching + retrying it as a transient outage (the churn dogfooding hit).
        "payment required",
        "payment method is required",
        "payment_required",
        "payment method to continue",
        "add a payment method",
    ];
    if MARKERS.iter().any(|m| l.contains(m)) {
        return true;
    }
    // Tool/function-calling unsupported, robust to punctuation/wording: a tool-or-function term
    // co-occurring with a "not supported / does not support" phrase. Catches e.g.
    // "`tool calling` is not supported with this model" and "model does not support tool use".
    //
    // PROXIMITY-gated: the two terms must be NEAR each other (same clause), not merely both present
    // somewhere in the body. Anywhere-co-occurrence produced false positives — e.g. "tool use works
    // fine, but JSON/structured-output mode is not supported" would wrongly mark the model as
    // permanently incapable of tool calling and exclude it for a week.
    const TOOL_TERMS: &[&str] = &[
        "tool calling",
        "tool use",
        "tool_use",
        "tool calls",
        "function calling",
        "function-calling",
        "function call",
    ];
    const UNSUPPORTED_TERMS: &[&str] = &[
        "not supported",
        "does not support",
        "isn't supported",
        "unsupported",
    ];
    const PROXIMITY: usize = 60;
    let tool_positions: Vec<usize> = TOOL_TERMS
        .iter()
        .flat_map(|t| l.match_indices(t).map(|(i, _)| i))
        .collect();
    if tool_positions.is_empty() {
        return false;
    }
    UNSUPPORTED_TERMS.iter().any(|u| {
        l.match_indices(u).any(|(up, _)| {
            tool_positions.iter().any(|&tp| {
                let (lo, hi) = if tp <= up { (tp, up) } else { (up, tp) };
                hi - lo <= PROXIMITY
            })
        })
    })
}

/// Classify from an HTTP status code. `body` is the raw provider response (inspected for
/// capability markers that a generic 400/404 status hides); `message` is the shortened display
/// string for the UI.
fn classify_status(
    code: u16,
    message: String,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> ProviderError {
    let exhausted = quota_is_exhausted(&message) || quota_is_exhausted(body);
    let message = short(&message);
    // A permanent incapability (no tool support / unaffordable) regardless of status code: 402
    // is always "can't afford", and 400/404 bodies often carry "tool calling not supported".
    if code == 402 || is_capability_failure(body) || is_capability_failure(&message) {
        return ProviderError::Capability(message);
    }
    match code {
        429 => ProviderError::RateLimited {
            message,
            retry_after: retry_after.filter(|_| !exhausted),
        },
        401 | 403 => ProviderError::Auth(message),
        500..=599 => ProviderError::Unavailable(message),
        _ => ProviderError::Request(message),
    }
}

/// Classify from a free-text error (the streaming case, where genai gives no typed status).
fn classify_text(text: &str, message: String) -> ProviderError {
    let lower = text.to_lowercase();
    let has = |needle: &str| lower.contains(needle);
    // retry_after is parsed from the full `text` before the message is shortened; a per-day /
    // zero quota drops the (useless) tiny delay so the longer default bench applies.
    let retry_after = parse_retry_after_body(text).filter(|_| !quota_is_exhausted(text));
    let message = short(&message);
    // Permanent incapability first — a streamed "tool calling is not supported" / "402 requires
    // more credits" must NOT be mistaken for a transient dropped stream (the misclassification
    // bug that benched-and-retried dead models forever).
    if is_capability_failure(text) {
        ProviderError::Capability(message)
    } else if has("429") || has("resource_exhausted") || has("rate limit") || has("quota") {
        ProviderError::RateLimited {
            message,
            retry_after,
        }
    } else if has(" 401") || has(" 403") || has("unauthorized") || has("permission denied") {
        ProviderError::Auth(message)
    } else if is_auth_config_failure(text) {
        // No-credentials / resolver failure surfaced via the streaming path — permanent (excluded),
        // not a transient outage.
        ProviderError::Auth(message)
    } else {
        // A dropped/5xx stream — treat as a transient provider problem worth failing over.
        ProviderError::Unavailable(message)
    }
}

/// Scan an error body for a cooldown: Gemini's `"retryDelay": "37s"` or a `retry in 37.04s`
/// phrase. Returns the first match.
fn parse_retry_after_body(body: &str) -> Option<std::time::Duration> {
    let lower = body.to_lowercase();
    for marker in ["retrydelay", "retry in", "retry after", "please retry in"] {
        if let Some(idx) = lower.find(marker) {
            if let Some(d) = parse_secs(&lower[idx + marker.len()..]).and_then(duration_from_secs) {
                return Some(d);
            }
        }
    }
    None
}

/// Build a `Duration` from a parsed seconds value, REJECTING non-finite / negative / absurd values
/// instead of panicking. `Duration::from_secs_f64` panics on NaN, infinity, a negative, or a value
/// too large to represent — an adversarial 429 body (`"retryDelay":"99999999999999999999s"`) would
/// otherwise crash the error-classification / failover path. Caps at a day; no sane cooldown is
/// longer, and clamping keeps a bogusly-huge hint from parking a model out of rotation forever.
fn duration_from_secs(secs: f64) -> Option<std::time::Duration> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    std::time::Duration::try_from_secs_f64(secs.min(86_400.0)).ok()
}

/// Pull the first floating-point number out of `s` (skipping leading quotes/colons/spaces),
/// e.g. `": \"37.04s\""` → `37.04`. Stops at the first non-numeric char after digits.
fn parse_secs(s: &str) -> Option<f64> {
    let mut num = String::new();
    let mut started = false;
    for c in s.chars() {
        // Accept a leading decimal point (`.5s`) too, and at most one dot.
        if c.is_ascii_digit() || (c == '.' && !num.contains('.')) {
            num.push(c);
            started = true;
        } else if started {
            break;
        } else if c == '"' || c == ':' || c == '=' || c.is_ascii_whitespace() {
            // Skip whitespace too (incl. `\n`/`\t`) — pretty-printed JSON puts a newline between the
            // key and value (`"retryDelay":\n  "37s"`), which used to abort the parse and drop the hint.
            continue;
        } else {
            // a non-numeric, non-separator char before any digit — give up.
            return None;
        }
    }
    num.parse::<f64>().ok()
}

fn model_benefits_from_effort(model: &str) -> bool {
    let m = model.to_lowercase();
    let is_openai_reasoning = ["o1", "o1-", "o3", "o3-", "o4", "o4-", "gpt-5"]
        .iter()
        .any(|needle| m == *needle || m.contains(&format!("::{needle}")) || m.contains(needle));

    is_openai_reasoning
        || m.contains("thinking")
        || m.contains("reasoning")
        || m.contains("deepseek-r1")
        || m.contains("r1-")
        || m == "deepseek-r1"
}

#[async_trait]
impl Provider for GenAiProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.complete_with(
            model,
            messages,
            tools,
            &CompletionOptions::default(),
            on_event,
        )
        .await
    }

    async fn complete_with(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &CompletionOptions,
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let model_name = to_genai_model(model);

        let mut genai_messages = to_genai_messages(messages);
        mark_cache_breakpoints(&mut genai_messages);
        let mut req = ChatRequest::new(genai_messages);
        if !tools.is_empty() {
            req = req.with_tools(tools.iter().map(to_genai_tool).collect::<Vec<_>>());
        }

        // Capture flags so the terminal End event carries usage + tool calls.
        let mut options = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true);
        // Bound the output so a free / low-credit account isn't billed (or 402'd) for a model's
        // full max-token default (mesh.max_output_tokens).
        if let Some(cap) = self.max_output_tokens {
            options = options.with_max_tokens(cap);
        }

        let mut reasoning_engaged = false;
        // Apply the caller's reasoning-effort hint when set (e.g. from `/effort high`).
        if let Some(effort) = opts.effort {
            if model_benefits_from_effort(&model_name) {
                let re = match effort {
                    EffortLevel::Low => ReasoningEffort::Low,
                    EffortLevel::Medium => ReasoningEffort::Medium,
                    EffortLevel::High => ReasoningEffort::High,
                    // Providers top out at xhigh — WhiteHot's extra lift is orchestration
                    // guidance in forge-core, not a provider knob.
                    EffortLevel::XHigh | EffortLevel::WhiteHot => ReasoningEffort::XHigh,
                };
                options = options.with_reasoning_effort(re);
                reasoning_engaged = true;
            }
        }

        if !reasoning_engaged {
            if let Some(temp) = opts.temperature {
                // Low temperature for deterministic edits/patches — but ONLY when reasoning isn't
                // engaged: thinking models reject (or ignore) a custom temperature, so effort wins.
                options = options.with_temperature(temp as f64);
            }
        }

        // Stall guards: a hung connection or a stream that goes silent must not freeze the
        // turn forever. A timeout surfaces as `Unavailable` (retryable), so the mesh fails over
        // to the next model instead of spinning indefinitely (model-health-failover).
        let first = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.client
                .exec_chat_stream(model_name.as_str(), req.clone(), Some(&options)),
        )
        .await
        .map_err(|_| stall_error("no response while connecting", CONNECT_TIMEOUT))?
        .map_err(|e| classify_genai_error(&e));

        // On 429: if this provider has ≥2 keys, retry once with the next key — the pool's
        // AtomicUsize has already advanced so the service-target resolver picks the other key
        // automatically. Surface the retry's result (success or any error) to the mesh.
        let res = match first {
            Err(ref e) if e.is_rate_limited() => {
                let provider = model.split("::").next().unwrap_or("");
                if self.pool.has_rotation(provider) {
                    // Retry with the next key — the pool's AtomicUsize has already advanced so
                    // the service-target resolver picks the other key automatically.
                    tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        self.client
                            .exec_chat_stream(model_name.as_str(), req, Some(&options)),
                    )
                    .await
                    .map_err(|_| stall_error("no response while connecting", CONNECT_TIMEOUT))?
                    .map_err(|e| classify_genai_error(&e))?
                } else if let Err(e) = first {
                    return Err(e);
                } else {
                    unreachable!()
                }
            }
            other => other?,
        };

        let mut stream = res.stream;
        let mut content = String::new();
        let mut usage = Usage::default();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Whether a proper finish/`End` event arrived. A stream that closes gracefully mid-generation
        // yields `next() == None` with NO `End` — partial text, no tool calls, no usage — which is
        // otherwise indistinguishable from a real completion. Track this so a silent truncation can
        // be rejected as retryable instead of returned as a phantom success (see below).
        let mut saw_end = false;

        // An *idle* timeout (per chunk), not a total cap: a long generation keeps emitting and
        // resets the clock; only a genuinely stalled stream trips it.
        while let Some(event) = tokio::time::timeout(IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| stall_error("stream stalled", IDLE_TIMEOUT))?
        {
            match event.map_err(|e| classify_genai_error(&e))? {
                ChatStreamEvent::Chunk(chunk) => {
                    content.push_str(&chunk.content);
                    on_event(StreamEvent::Text(chunk.content.clone()));
                }
                ChatStreamEvent::ReasoningChunk(chunk) => {
                    // Extended-thinking delta: streamed for display, not part of the answer.
                    on_event(StreamEvent::Reasoning(chunk.content.clone()));
                }
                ChatStreamEvent::End(end) => {
                    saw_end = true;
                    if let Some(u) = &end.captured_usage {
                        // Cache-read tokens (subset of prompt_tokens) are billed at a fraction of
                        // the input rate; capture them so the mesh prices them correctly instead of
                        // charging the full rate (which diverges from the provider's actual bill).
                        let cached = u
                            .prompt_tokens_details
                            .as_ref()
                            .and_then(|d| d.cached_tokens)
                            .unwrap_or(0)
                            .max(0) as u64;
                        usage = Usage {
                            input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u64,
                            output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u64,
                            cached_input_tokens: cached,
                            cost_usd: 0.0, // priced by the mesh from token counts (FR-5)
                        };
                    }
                    // Some providers deliver text only at the end (not chunked).
                    if content.is_empty() {
                        if let Some(text) = end.captured_first_text() {
                            content.push_str(text);
                            on_event(StreamEvent::Text(text.to_string()));
                        }
                    }
                    if let Some(tcs) = end.captured_into_tool_calls() {
                        tool_calls = tcs
                            .into_iter()
                            .map(|tc| ToolCall {
                                id: tc.call_id,
                                name: tc.fn_name,
                                // genai falls back to the raw accumulated string when its own
                                // end-of-stream parse fails (a dropped/duplicated chunk); repair
                                // it here so a malformed call can never be stored/replayed as-is
                                // (see `repair_malformed_args` doc comment for why that matters).
                                args: crate::repair_malformed_args(tc.fn_arguments),
                            })
                            .collect();
                    }
                }
                _ => {}
            }
        }

        // Recovery pass: some native adapters (e.g. genai's Gemini adapter on newer models) don't
        // decode the model's function calls into structured tool_calls — they leak into `content`
        // as `<invoke>`/`<tool_call>` markup. Without this, Forge sees no tool calls, treats the
        // narration as a final answer, and "succeeds" without acting (the phantom-release bug). When
        // the structured capture came back empty, reconstruct calls from the text and strip the
        // markup so the visible content stays clean.
        if tool_calls.is_empty() {
            let (recovered, cleaned) = crate::recover_text_tool_calls(&content);
            if !recovered.is_empty() {
                tool_calls = recovered;
                content = cleaned;
            }
        }

        // Phantom-success guard: if the stream ended WITHOUT any completion signal (no `End`/finish
        // event, no usage) AND produced no tool calls, it was almost certainly truncated mid-flight
        // (e.g. the connection closed cleanly just before a tool call). Returning the partial
        // narration as `Ok` would make the mesh treat it as a final answer and stop acting. Surface
        // a retryable error instead so failover/retry kicks in. A legitimately empty-but-finished
        // response (an `End` event arrived) still succeeds.
        if is_phantom_truncation(saw_end, !tool_calls.is_empty(), &usage) {
            return Err(ProviderError::Unavailable(
                "stream closed without a completion signal (truncated mid-generation)".to_string(),
            ));
        }

        Ok(ModelResponse {
            content,
            tool_calls,
            usage,
            quotas: Vec::new(),
        })
    }
}

/// Decide whether a finished stream is a silently-truncated phantom rather than a real completion.
/// True (→ reject as retryable) only when NONE of the completion signals are present: no finish/
/// `End` event, no tool calls, and zero usage. Any one of those means the provider actually
/// finished (even an empty answer), so the response is kept.
fn is_phantom_truncation(saw_end: bool, has_tool_calls: bool, usage: &Usage) -> bool {
    !saw_end && !has_tool_calls && usage.input_tokens == 0 && usage.output_tokens == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_azure(endpoint: &str, env_var: &str) -> AzureProvider {
        AzureProvider {
            endpoint: endpoint.to_string(),
            api_version: "2024-10-21".to_string(),
            env_var: env_var.to_string(),
            deployments: vec!["gpt4o".to_string()],
            free: false,
            label: String::new(),
        }
    }

    #[test]
    fn azure_service_target_builds_override_url_and_api_key_header() {
        let azure = test_azure("https://res.openai.azure.com", "AZURE_OPENAI_API_KEY");
        let st = azure_service_target(&azure, "gpt4o", "sk-secret");
        match st.auth {
            AuthData::RequestOverride { url, headers } => {
                assert_eq!(
                    url,
                    "https://res.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
                );
                let api_key = headers.iter().find(|(k, _)| k.as_str() == "api-key");
                assert_eq!(api_key.map(|(_, v)| v.as_str()), Some("sk-secret"));
                // Azure authenticates with `api-key`, never `Authorization: Bearer`.
                assert!(headers.iter().all(|(k, _)| k.as_str() != "Authorization"));
            }
            other => panic!("expected RequestOverride, got {other:?}"),
        }
        // The OpenAI adapter shapes the (standard) request body; deployment is the body model.
        assert_eq!(st.model.model_name.as_str(), "gpt4o");
    }

    #[tokio::test]
    async fn azure_routes_through_genai_to_the_deployment_url_with_api_key() {
        // Mock the HTTP layer — never hit real Azure. Prove an `azure::<deployment>` id is routed by
        // the resolver to Azure's deployment-scoped URL, with the `api-key` header and the
        // `api-version` query, and that the standard OpenAI response body round-trips back.
        let server = httpmock::MockServer::start();
        let env_var = "FORGE_AZURE_TEST_KEY";
        std::env::set_var(env_var, "sk-azure-mock");

        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/openai/deployments/gpt4o/chat/completions")
                .query_param("api-version", "2024-10-21")
                .header("api-key", "sk-azure-mock");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "model": "gpt4o",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hello from azure"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
                }));
        });

        let azure = test_azure(&server.base_url(), env_var);
        let client = build_client_full(std::sync::Arc::new(KeyPool::default()), Some(azure));
        let req = ChatRequest::new(vec![ChatMessage::user("hi")]);
        let res = client
            .exec_chat("azure::gpt4o", req, None)
            .await
            .expect("azure chat completes against the mock");

        std::env::remove_var(env_var);
        mock.assert(); // the deployment URL + api-key header + api-version query were all matched
        assert_eq!(
            res.first_text().map(str::to_string).as_deref(),
            Some("hello from azure")
        );
    }

    #[test]
    fn ollama_v1_endpoint_normalizes_host_forms() {
        assert_eq!(
            ollama_v1_endpoint("http://localhost:11434"),
            "http://localhost:11434/v1/"
        );
        // Bare host (ollama's common OLLAMA_HOST form) gets a scheme.
        assert_eq!(
            ollama_v1_endpoint("127.0.0.1:11434"),
            "http://127.0.0.1:11434/v1/"
        );
        // A trailing slash isn't doubled.
        assert_eq!(
            ollama_v1_endpoint("http://box:11434/"),
            "http://box:11434/v1/"
        );
    }

    #[test]
    fn cache_breakpoints_mark_system_and_last_message() {
        let msgs = [
            Message::system("you are forge"),
            Message::user("hi"),
            Message::assistant("hello"),
            Message::user("fix this bug"),
        ];
        let mut genai = to_genai_messages(&msgs);
        mark_cache_breakpoints(&mut genai);
        // System (idx 0) and final user message (idx 3) carry a cache breakpoint; the middle
        // turns don't, so the cache reads the largest stable prefix.
        assert!(genai[0].options.is_some(), "system should be a breakpoint");
        assert!(genai[1].options.is_none());
        assert!(genai[2].options.is_none());
        assert!(
            genai[genai.len() - 1].options.is_some(),
            "last message should be a breakpoint"
        );
    }

    #[test]
    fn cache_breakpoint_anchors_on_the_last_leading_system_message() {
        // Forge stacks several system messages (base prompt + env + AGENTS.md). The breakpoint must
        // sit on the LAST of them so the whole standing prefix is cached, not just the first.
        let msgs = [
            Message::system("base prompt"),
            Message::system("<env>"),
            Message::system("AGENTS.md"),
            Message::user("do it"),
        ];
        let mut genai = to_genai_messages(&msgs);
        mark_cache_breakpoints(&mut genai);
        assert!(genai[0].options.is_none(), "not just the first system");
        assert!(genai[1].options.is_none());
        assert!(
            genai[2].options.is_some(),
            "last leading system carries the breakpoint"
        );
        assert!(genai[3].options.is_some(), "final message too");
    }

    #[test]
    fn cache_breakpoints_on_empty_is_noop() {
        let mut empty: Vec<ChatMessage> = Vec::new();
        mark_cache_breakpoints(&mut empty);
        assert!(empty.is_empty());
    }

    /// Live reproduction (needs an OpenRouter key; run with `--ignored`). Proves both halves of the
    /// context-overflow diagnosis against the SAME model from the failed turn
    /// (`cohere/north-mini-code:free`, which appeared as "unavailable" in the user's log):
    ///   1. an oversized prompt fails as a retryable `Unavailable` (the cascade cause), and
    ///   2. an in-window prompt to that same model succeeds (the model itself is healthy — the
    ///      fix is to trim to the window, not to avoid the model).
    /// `cargo test -p forge-provider -- --ignored openrouter_overflow`.
    #[tokio::test]
    #[ignore = "hits the live OpenRouter API; needs a key"]
    async fn openrouter_overflow_is_retryable_then_in_window_succeeds() {
        forge_config::inject_provider_keys();
        let provider = GenAiProvider::new().with_max_output_tokens(64);
        let model = "openrouter::cohere/north-mini-code:free";
        let mut sink = |_: StreamEvent| {};

        // 1. Overflow: ~1.6M chars ≈ 400k tokens, well over this model's ~256k window.
        let big = [Message::user("word ".repeat(330_000))];
        let err = provider
            .complete(model, &big, &[], &mut sink)
            .await
            .expect_err("an oversized prompt must fail");
        assert!(
            err.is_retryable() && !err.is_permanent(),
            "overflow should fail over (transient), got: {err:?}"
        );

        // 2. In-window: a normal prompt to the SAME model answers fine.
        let ok = [Message::user("Reply with the single word: pong")];
        let resp = provider
            .complete(model, &ok, &[], &mut sink)
            .await
            .expect("an in-window prompt to a healthy free model should succeed");
        assert!(
            !resp.content.trim().is_empty(),
            "expected a non-empty reply, got empty"
        );
    }

    #[test]
    fn to_genai_model_passes_namespaced_ids_through() {
        // Native providers: namespace kept verbatim so genai selects the adapter explicitly.
        assert_eq!(to_genai_model("ollama::llama3.2"), "ollama::llama3.2");
        assert_eq!(to_genai_model("openai::gpt-4o"), "openai::gpt-4o");
        assert_eq!(
            to_genai_model("anthropic::claude-opus-4-8"),
            "anthropic::claude-opus-4-8"
        );
        assert_eq!(
            to_genai_model("gemini::gemini-2.5-pro"),
            "gemini::gemini-2.5-pro"
        );
        assert_eq!(to_genai_model("xai::grok-4"), "xai::grok-4");
        assert_eq!(
            to_genai_model("deepseek::deepseek-chat"),
            "deepseek::deepseek-chat"
        );
    }

    #[test]
    fn to_genai_model_renames_openrouter_alias() {
        // Forge says `openrouter`; genai's namespace is `open_router`. The model part —
        // including its `/` and any later separators — is preserved.
        assert_eq!(
            to_genai_model("openrouter::deepseek/deepseek-chat"),
            "open_router::deepseek/deepseek-chat"
        );
        assert_eq!(
            to_genai_model("openrouter::anthropic/claude-sonnet-4-5"),
            "open_router::anthropic/claude-sonnet-4-5"
        );
    }

    #[test]
    fn to_genai_model_passes_free_provider_namespaces_through() {
        // genai 0.6 has native adapters for these — pass the namespaced id straight through.
        for (input, expect) in [
            (
                "groq::llama-3.3-70b-versatile",
                "groq::llama-3.3-70b-versatile",
            ),
            (
                "opencode_go::deepseek-v4-flash",
                "opencode_go::deepseek-v4-flash",
            ),
            (
                "github_copilot::openai/gpt-4.1-mini",
                "github_copilot::openai/gpt-4.1-mini",
            ),
            ("mimo::mimo-v2.5", "mimo::mimo-v2.5"),
            // Cerebras has no native adapter: the id stays namespaced so the client's
            // service-target resolver can detect `cerebras` and retarget the OpenAI endpoint.
            ("cerebras::llama-3.3-70b", "cerebras::llama-3.3-70b"),
        ] {
            assert_eq!(to_genai_model(input), expect);
        }
    }

    #[test]
    fn to_genai_model_splits_on_first_separator() {
        // genai splits namespace on the FIRST `::`, so the remainder stays intact.
        assert_eq!(to_genai_model("openai::a::b"), "openai::a::b");
    }

    #[test]
    fn cerebras_is_not_discoverable_but_adapter_backed_providers_are() {
        // Cerebras is completion-only (custom resolver, no native adapter) → not auto-discoverable,
        // so the discovery loop skips it quietly instead of warning "check your key".
        assert!(!is_discoverable("cerebras"));
        // Providers genai has a native adapter for CAN be listed.
        assert!(is_discoverable("anthropic"));
        assert!(is_discoverable("openai"));
        assert!(is_discoverable("groq"));
        // The OpenRouter alias normalizes to its adapter too.
        assert!(is_discoverable("openrouter"));
    }

    #[test]
    fn key_pool_round_robins_and_skips_single_key_providers() {
        use std::sync::atomic::AtomicUsize;
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "groq".to_string(),
            (
                vec!["a".to_string(), "b".to_string(), "c".to_string()],
                AtomicUsize::new(0),
            ),
        );
        let pool = KeyPool { providers };
        // Round-robins across the three keys and wraps.
        assert_eq!(pool.next("groq").as_deref(), Some("a"));
        assert_eq!(pool.next("groq").as_deref(), Some("b"));
        assert_eq!(pool.next("groq").as_deref(), Some("c"));
        assert_eq!(pool.next("groq").as_deref(), Some("a"));
        // A provider not in the pool (≤1 key) yields None → genai env default is used unchanged.
        assert_eq!(pool.next("gemini"), None);
    }

    #[test]
    fn to_genai_model_without_prefix_is_verbatim() {
        assert_eq!(to_genai_model("claude-3-5-sonnet"), "claude-3-5-sonnet");
        assert_eq!(to_genai_model(""), "");
    }

    #[test]
    fn maps_all_roles_and_round_trips_tool_call_ids() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant_tool_calls(
                "thinking",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "x"}),
                }],
            ),
            Message::tool_result("call_1", "file contents"),
        ];
        let out = to_genai_messages(&msgs);
        // system, user, assistant-text, assistant-tool-call, tool-response = 5
        assert_eq!(out.len(), 5, "every role maps to a genai message");
    }

    #[test]
    fn empty_assistant_content_emits_no_stray_text_message() {
        // assistant with empty content but a tool call -> only the tool-call message.
        let msgs = vec![Message::assistant_tool_calls(
            "",
            vec![ToolCall {
                id: "c".into(),
                name: "t".into(),
                args: json!({}),
            }],
        )];
        let out = to_genai_messages(&msgs);
        assert_eq!(out.len(), 1, "no empty assistant text message");
    }

    // --- Error classification + retry-after parsing (model-health-failover) ---

    // The exact 429 body from the bug report (truncated to the parts that matter).
    const GEMINI_429: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details. Quota exceeded for metric: ... limit: 0, model: antigravity. Please retry in 37.047405996s.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"37s"}]}}"#;

    #[test]
    fn exhausted_quota_429_drops_the_useless_short_delay() {
        // GEMINI_429 is `limit: 0` (free tier disabled) — its 37s retryDelay would just thrash,
        // so retry_after is dropped and the caller's longer default bench applies.
        let e = classify_text(GEMINI_429, "stream err".into());
        match e {
            ProviderError::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert!(e.is_retryable());
    }

    #[test]
    fn resolver_error_no_key_classes_as_permanent_auth() {
        // genai's no-credentials failure ("Resolver error for model 'groq::…'") must be PERMANENT
        // (Auth → excluded), not transient — otherwise the last-resort path re-benches the keyless
        // model forever (the "groq for everything" report).
        let msg = "Resolver error for model 'groq::llama-3.3-70b-versatile (adapter: Groq)'";
        let e = classify_text(msg, msg.into());
        assert!(matches!(e, ProviderError::Auth(_)), "got {e:?}");
        assert!(e.is_permanent());
        assert!(is_auth_config_failure(msg));
        assert!(!is_auth_config_failure(
            "provider unavailable: 502 bad gateway"
        ));
    }

    #[test]
    fn transient_per_minute_429_keeps_its_server_delay() {
        // A genuine per-minute limit (no limit:0 / per-day) honors the short retryDelay.
        let body = r#"{"error":{"code":429,"message":"rate limit, retry soon","status":"RESOURCE_EXHAUSTED","details":[{"@type":"...RetryInfo","retryDelay":"12s"}]}}"#;
        match classify_text(body, "stream err".into()) {
            ProviderError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(12)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn parse_retry_after_does_not_panic_on_malformed_values() {
        // The bug: an unbounded digit run fed straight to `Duration::from_secs_f64`, which PANICS
        // on a too-large / non-finite value. These adversarial 429 bodies must yield None (or a
        // clamped, sane Duration), never crash the failover path.
        for body in [
            r#""retryDelay": "99999999999999999999s""#, // absurdly large → clamped/None, no panic
            r#""retryDelay": "NaN""#,
            r#""retryDelay": "-5""#,
            r#""retryDelay": "inf""#,
            r#""retryDelay": "1e400s""#,
        ] {
            let d = parse_retry_after_body(body);
            // Whatever it returns, it must be a finite, day-bounded Duration (or None) — never panic.
            if let Some(d) = d {
                assert!(
                    d <= std::time::Duration::from_secs(86_400),
                    "retry-after must be clamped, got {d:?} for {body:?}"
                );
            }
        }
        // A huge value is dropped or clamped, not honored as-is.
        assert!(
            parse_retry_after_body(r#""retryDelay": "99999999999999999999s""#)
                .map(|d| d <= std::time::Duration::from_secs(86_400))
                .unwrap_or(true)
        );
        // duration_from_secs rejects the non-finite / negative cases outright.
        assert_eq!(duration_from_secs(f64::NAN), None);
        assert_eq!(duration_from_secs(f64::INFINITY), None);
        assert_eq!(duration_from_secs(-1.0), None);
        assert_eq!(
            duration_from_secs(1e30),
            Some(std::time::Duration::from_secs(86_400)),
            "absurdly large values clamp to the day cap"
        );
    }

    #[test]
    fn phantom_truncation_rejects_silent_close_keeps_real_completions() {
        let zero = Usage::default();
        // No End, no tool calls, no usage → silent truncation, reject (retryable).
        assert!(is_phantom_truncation(false, false, &zero));
        // A finish/End event arrived → legit, even with an empty/zero-usage answer.
        assert!(!is_phantom_truncation(true, false, &zero));
        // Tool calls present (e.g. recovered from text) → legit even without an End event.
        assert!(!is_phantom_truncation(false, true, &zero));
        // Non-zero usage is itself a completion signal → legit.
        let used = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        };
        assert!(!is_phantom_truncation(false, false, &used));
    }

    #[test]
    fn parse_retry_after_reads_retrydelay_and_retry_in() {
        assert_eq!(
            parse_retry_after_body(r#""retryDelay": "37s""#),
            Some(std::time::Duration::from_secs(37))
        );
        let d = parse_retry_after_body("Please retry in 37.047405996s.").unwrap();
        assert!((d.as_secs_f64() - 37.047405996).abs() < 1e-6, "{d:?}");
        assert_eq!(parse_retry_after_body("no cooldown here"), None);
    }

    #[test]
    fn short_keeps_first_line_and_drops_the_json_body() {
        // The real failure: the whole HTTP body was flooding the UI. `short` must cut it.
        let s = classify_text(
            GEMINI_429,
            format!("Web stream error for model 'gemini'.\nBody: {GEMINI_429}"),
        );
        let msg = s.to_string();
        assert!(!msg.contains('{'), "no JSON body in the message: {msg}");
        assert!(
            msg.chars().count() < 200,
            "message is short: {} chars",
            msg.chars().count()
        );
    }

    #[test]
    fn stall_error_is_retryable_unavailable() {
        let e = stall_error("stream stalled", std::time::Duration::from_secs(90));
        assert!(matches!(e, ProviderError::Unavailable(_)));
        assert!(e.is_retryable(), "a stall must fail over");
        assert!(e.to_string().contains("90s"));
    }

    #[test]
    fn classify_status_maps_codes() {
        let none = None;
        assert!(matches!(
            classify_status(429, "x".into(), "", none),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_status(401, "x".into(), "", None),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            classify_status(503, "x".into(), "", None),
            ProviderError::Unavailable(_)
        ));
        // 400 misuse is non-retryable — must not fail over.
        let bad = classify_status(400, "x".into(), "", None);
        assert!(matches!(bad, ProviderError::Request(_)));
        assert!(!bad.is_retryable());
    }

    #[test]
    fn capability_failures_are_permanent_and_fail_over() {
        // 402 (can't afford) → permanent exclusion, but still fails over to another model.
        let credit = classify_status(402, "x".into(), "requires more credits", None);
        assert!(matches!(credit, ProviderError::Capability(_)));
        assert!(credit.is_permanent());
        assert!(
            credit.is_retryable(),
            "must still fail over to another model"
        );

        // 400 body that names a tool-support problem → Capability, not a plain Request.
        let no_tools = classify_status(
            400,
            "x".into(),
            "`tool calling` is not supported with this model",
            None,
        );
        assert!(matches!(no_tools, ProviderError::Capability(_)));
        assert!(no_tools.is_permanent());

        // Streaming path: the same markers in free text.
        for body in [
            "No endpoints found that support tool use",
            "function name or parameters is empty (2013)",
            "model does not support feature: function-calling",
        ] {
            let e = classify_text(body, body.to_string());
            assert!(
                matches!(e, ProviderError::Capability(_)),
                "expected Capability for {body:?}, got {e:?}"
            );
            assert!(e.is_permanent());
        }

        // A genuine dropped stream is still transient (not permanent).
        let dropped = classify_text("connection reset by peer", "stream dropped".into());
        assert!(matches!(dropped, ProviderError::Unavailable(_)));
        assert!(!dropped.is_permanent());
    }

    #[test]
    fn capability_failure_requires_tool_and_unsupported_to_be_near() {
        // Near each other (same clause) → genuine capability failure.
        assert!(is_capability_failure(
            "`tool calling` is not supported with this model"
        ));
        assert!(is_capability_failure(
            "this model does not support tool use"
        ));
        assert!(is_capability_failure("unsupported: function calling"));

        // Both terms present but FAR apart in unrelated clauses → NOT a capability failure (the old
        // anywhere-match bug would have wrongly excluded the model for a week).
        assert!(!is_capability_failure(
            "Tool use works fine on this model. However, JSON / structured-output response_format \
             mode is not supported for the requested configuration."
        ));
        // "unsupported" about something unrelated, with tools merely mentioned far earlier.
        assert!(!is_capability_failure(
            "Your tool calls executed successfully and were applied to the working tree without \
             any error at all. As a separate and entirely unrelated note, the deprecated \
             temperature override flag is unsupported."
        ));
    }

    #[test]
    fn cooldown_prefers_server_value_then_default() {
        let rl = ProviderError::RateLimited {
            message: "x".into(),
            retry_after: Some(std::time::Duration::from_secs(37)),
        };
        assert_eq!(
            rl.cooldown(std::time::Duration::from_secs(300)),
            std::time::Duration::from_secs(37)
        );
        let un = ProviderError::Unavailable("x".into());
        assert_eq!(
            un.cooldown(std::time::Duration::from_secs(300)),
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn tool_spec_maps_name_description_and_schema() {
        let schema = json!({"type":"object","properties":{"path":{"type":"string"}}});
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "read a file".into(),
            schema: schema.clone(),
        };
        let tool = to_genai_tool(&spec);
        assert_eq!(tool.name.as_str(), "read_file");
        assert_eq!(tool.description.as_deref(), Some("read a file"));
        assert_eq!(tool.schema.as_ref(), Some(&schema));
    }

    // --- Enterprise / custom-endpoint plumbing ---

    #[test]
    fn bedrock_namespace_maps_to_genai_bedrock_api_and_vertex_passes_through() {
        // `bedrock::…` must resolve to genai's Bearer-token Bedrock adapter (`bedrock_api`); Vertex
        // keeps its namespace as-is.
        assert_eq!(
            to_genai_model("bedrock::anthropic.claude-sonnet-4-5-v1:0"),
            "bedrock_api::anthropic.claude-sonnet-4-5-v1:0"
        );
        assert_eq!(
            to_genai_model("vertex::gemini-2.5-pro"),
            "vertex::gemini-2.5-pro"
        );
        // And the genai adapter actually exists for the mapped name.
        assert!(AdapterKind::from_lower_str("bedrock_api").is_some());
        assert!(AdapterKind::from_lower_str("vertex").is_some());
    }

    #[test]
    fn enterprise_gateways_are_not_listable_but_still_adapter_backed() {
        // Bedrock/Vertex route via genai but have no enumerable models endpoint for our flow → they
        // are skipped quietly in discovery (users pin ids), not warned about as a key failure.
        assert!(!is_discoverable("bedrock"));
        assert!(!is_discoverable("vertex"));
        // Regular providers stay discoverable.
        assert!(is_discoverable("openai"));
        assert!(is_discoverable("groq"));
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_timeout_elapses_to_retryable_unavailable() {
        // A discovery call that never returns must not hang the caller — `with_discovery_timeout`
        // bounds it and surfaces a retryable Unavailable. `start_paused` auto-advances virtual time
        // to the next timer once the task is idle, so this is deterministic and instant.
        let hung = async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok::<(), ProviderError>(())
        };
        let e = with_discovery_timeout("model listing", hung)
            .await
            .expect_err("a hung discovery call must time out");
        assert!(matches!(e, ProviderError::Unavailable(_)));
        assert!(e.is_retryable(), "a discovery timeout should be retryable");
        assert!(e.to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_timeout_passes_through_a_fast_result() {
        let ok = async { Ok::<u8, ProviderError>(7) };
        assert_eq!(with_discovery_timeout("x", ok).await.unwrap(), 7);
    }

    // --- Per-provider error-classification contract tests ---
    //
    // Feed representative REAL error-body shapes (the typed JSON each vendor returns) through the
    // structured classifier and the text/status fallbacks, asserting the category. A future provider
    // rephrasing its prose is caught here because the typed field still carries the signal.

    #[test]
    fn contract_openai_429_402_401_via_structured_body() {
        // OpenAI rate limit (string code + type).
        let rl = json!({"error":{"message":"Rate limit reached for gpt-4o","type":"requests","code":"rate_limit_exceeded"}});
        assert!(matches!(
            classify_error_body(&rl),
            Some(ProviderError::RateLimited { .. })
        ));
        // OpenAI out-of-quota (billing) → permanent Capability.
        let quota = json!({"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}});
        let q = classify_error_body(&quota).expect("classified");
        assert!(matches!(q, ProviderError::Capability(_)));
        assert!(q.is_permanent());
        // OpenAI bad key.
        let auth = json!({"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}});
        assert!(matches!(
            classify_error_body(&auth),
            Some(ProviderError::Auth(_))
        ));
    }

    #[test]
    fn contract_anthropic_overloaded_ratelimit_auth() {
        for (body, want_rl) in [
            (
                json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
                true,
            ),
            (
                json!({"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}),
                true,
            ),
        ] {
            let e = classify_error_body(&body).expect("classified");
            assert_eq!(
                matches!(e, ProviderError::RateLimited { .. }),
                want_rl,
                "{body}"
            );
        }
        let auth = json!({"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}});
        assert!(matches!(
            classify_error_body(&auth),
            Some(ProviderError::Auth(_))
        ));
    }

    #[test]
    fn contract_gemini_resource_exhausted_and_unauthenticated() {
        let rl = json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded. Please retry in 12s."}});
        match classify_error_body(&rl).expect("classified") {
            // code 429 path → RateLimited (the 12s server hint survives, not a limit:0 quota).
            ProviderError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(12)))
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // No numeric code, only the RPC status enum.
        let auth = json!({"error":{"status":"UNAUTHENTICATED","message":"API key not valid"}});
        assert!(matches!(
            classify_error_body(&auth),
            Some(ProviderError::Auth(_))
        ));
    }

    #[test]
    fn contract_groq_429_and_capability_via_status_and_text() {
        // Groq returns an OpenAI-shaped 429 body.
        let rl = json!({"error":{"message":"Rate limit reached","type":"rate_limit_exceeded","code":"rate_limit_exceeded"}});
        assert!(matches!(
            classify_error_body(&rl),
            Some(ProviderError::RateLimited { .. })
        ));
        // Groq llama refusing tool use, leaked on the stream path as free text → Capability.
        let cap = classify_text(
            "tool use is not supported with this model",
            "stream err".into(),
        );
        assert!(matches!(cap, ProviderError::Capability(_)));
        assert!(cap.is_permanent());
    }

    #[test]
    fn contract_openrouter_402_and_no_tool_endpoints() {
        // 402 numeric code in the structured body → permanent Capability.
        let pay = json!({"error":{"code":402,"message":"This request requires more credits, or fewer max_tokens."}});
        let e = classify_error_body(&pay).expect("classified");
        assert!(matches!(e, ProviderError::Capability(_)));
        assert!(e.is_permanent());
        // The classic "no endpoints found that support tool use".
        let cap =
            json!({"error":{"message":"No endpoints found that support tool use.","code":404}});
        assert!(matches!(
            classify_error_body(&cap),
            Some(ProviderError::Capability(_))
        ));
    }

    #[test]
    fn contract_sambanova_payment_required_on_stream_path() {
        // SambaNova surfaces 402 as free text on the streaming path → Capability, not a transient.
        let e = classify_text(
            "A payment method is required to use `Meta-Llama-3.3-70B-Instruct`",
            "stream err".into(),
        );
        assert!(matches!(e, ProviderError::Capability(_)));
        assert!(e.is_permanent());
    }

    #[test]
    fn contract_webstream_cause_with_embedded_json_is_classified_structurally() {
        // The streaming `cause` often embeds the JSON body after a prefix — extract + classify it.
        let cause = "Web stream error. Body: {\"error\":{\"code\":429,\"status\":\"RESOURCE_EXHAUSTED\",\"message\":\"slow down\"}}";
        let v = parse_embedded_json(cause).expect("embedded json found");
        assert!(matches!(
            classify_error_body(&v),
            Some(ProviderError::RateLimited { .. })
        ));
    }

    #[test]
    fn json_status_code_accepts_int_and_numeric_string_only() {
        assert_eq!(json_status_code(&json!(429)), Some(429));
        assert_eq!(json_status_code(&json!("503")), Some(503));
        assert_eq!(json_status_code(&json!("rate_limit_exceeded")), None);
        assert_eq!(json_status_code(&json!(20000)), None); // out of range
    }

    #[test]
    fn classify_error_body_returns_none_for_unrecognized_shape() {
        // A body with no recognizable typed signal → None so the caller falls back to text scanning.
        let v = json!({"weird":"shape","detail":123});
        assert!(classify_error_body(&v).is_none());
    }

    // --- tool_recovery contract tests: the exact per-provider leak formats ---

    #[test]
    fn contract_tool_recovery_known_leak_formats() {
        use crate::recover_text_tool_calls;
        // Gemini-style <invoke> leak.
        let (c, _) = recover_text_tool_calls(
            "<invoke name=\"shell\"><parameter name=\"command\">ls</parameter></invoke>",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "shell");
        // Qwen/ollama-style <tool_call> JSON leak.
        let (c, _) = recover_text_tool_calls(
            "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"x\"}}</tool_call>",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "read_file");
        // Llama/Groq-style <function=…> leak, optionally wrapped in <tool_call>.
        let (c, _) = recover_text_tool_calls(
            "<tool_call><function=shell>{\"command\":\"ls\"}</function></tool_call>",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "shell");
    }
}
