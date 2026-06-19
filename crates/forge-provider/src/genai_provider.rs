//! `genai`-backed implementation of [`Provider`] (ADR-0003). genai 0.6 resolves an adapter
//! per `namespace::model` id, so this one backend covers Anthropic, OpenAI, Gemini, xAI,
//! DeepSeek, OpenRouter, Groq, OpenCode Zen (`opencode_go`), GitHub Models, MiMo, MiniMax,
//! Ollama, … plus Cerebras via a custom-endpoint resolver ([`build_client`]). Tool calling is
//! normalized: tools are advertised, the model's calls map back to Forge [`ToolCall`]s, and
//! prior tool results are replayed as genai tool responses so multi-step loops round-trip.

use async_trait::async_trait;
use forge_types::{EffortLevel, Message, Role, ToolCall, Usage};
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    Binary, CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent,
    ContentPart, MessageContent, ReasoningEffort, Tool, ToolCall as GenAiToolCall, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

use crate::{
    CompletionOptions, EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec,
};

#[derive(Default)]
pub struct GenAiProvider {
    client: Client,
    /// Per-completion output cap (`mesh.max_output_tokens`). `None` → no cap (provider default,
    /// often a model's full 65k max — too much for a free/low-credit account, see the 402 churn).
    max_output_tokens: Option<u32>,
}

impl GenAiProvider {
    pub fn new() -> Self {
        Self {
            client: build_client(),
            max_output_tokens: None,
        }
    }

    /// Construct with a caller-supplied `genai::Client`. Used by the HTTP contract tests to
    /// point genai at a local mock server; otherwise identical to [`GenAiProvider::new`].
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            max_output_tokens: None,
        }
    }

    /// Cap the output tokens requested per completion. `0` disables the cap (provider default).
    pub fn with_max_output_tokens(mut self, cap: u32) -> Self {
        self.max_output_tokens = (cap > 0).then_some(cap);
        self
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
    let names = Client::default()
        .all_model_names(kind, None)
        .await
        .map_err(|e| ProviderError::Request(e.to_string()))?;
    // Re-namespace with Forge's provider name (so `openrouter` stays `openrouter::…`).
    Ok(names
        .into_iter()
        .map(|n| format!("{namespace}::{n}"))
        .collect())
}

/// Build the genai client with a custom-endpoint resolver for providers genai has no native
/// adapter for. Today that's **Cerebras** (`cerebras::<model>`): an OpenAI-compatible API at
/// `api.cerebras.ai`, keyed by `CEREBRAS_API_KEY`. genai keeps the full `cerebras::…` string as
/// the model name (unknown namespace → Ollama fallback), so the resolver detects the `cerebras`
/// namespace, strips it, and retargets the OpenAI adapter + Cerebras endpoint + key. All native
/// namespaces (groq/gemini/open_router/opencode_go/github_copilot/mimo/minimax/…) pass through
/// unchanged.
pub(crate) fn build_client() -> Client {
    let resolver = ServiceTargetResolver::from_resolver_fn(
        |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            let is_cerebras = st.model.model_name.namespace_is("cerebras");
            if !is_cerebras {
                return Ok(st);
            }
            let bare = st.model.model_name.namespace_and_name().1.to_string();
            Ok(ServiceTarget {
                endpoint: Endpoint::from_static("https://api.cerebras.ai/v1/"),
                auth: AuthData::from_env("CEREBRAS_API_KEY"),
                model: ModelIden::new(AdapterKind::OpenAI, bare),
            })
        },
    );
    Client::builder()
        .with_service_target_resolver(resolver)
        .build()
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

/// Map a Forge provider prefix to the namespace genai expects. Identity for everything
/// except `openrouter`, which genai spells `open_router`.
fn normalize_namespace(prefix: &str) -> &str {
    match prefix {
        "openrouter" => "open_router",
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
    let sys = msgs.iter().position(|m| m.role == ChatRole::System);
    let last = msgs.len() - 1;
    for idx in [sys, Some(last)].into_iter().flatten() {
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
                            .or_else(|| parse_secs(t).map(std::time::Duration::from_secs_f64))
                    })
                    .or_else(|| parse_retry_after_body(body));
                classify_status(status.as_u16(), err.to_string(), body, retry_after)
            }
            other => ProviderError::Unavailable(short(&other.to_string())),
        },
        // Streaming path: status lives only in the message string (`...Status: 429...`).
        genai::Error::WebStream { cause, .. } => classify_text(cause, err.to_string()),
        genai::Error::ChatResponse { body, .. } => {
            classify_text(&body.to_string(), err.to_string())
        }
        // A bad/truncated stream chunk — transient, worth trying elsewhere.
        genai::Error::StreamParse { .. } => ProviderError::Unavailable(short(&err.to_string())),
        other => ProviderError::Request(short(&other.to_string())),
    }
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
    ];
    if MARKERS.iter().any(|m| l.contains(m)) {
        return true;
    }
    // Tool/function-calling unsupported, robust to punctuation/wording: a tool-or-function term
    // co-occurring with a "not supported / does not support" phrase. Catches e.g.
    // "`tool calling` is not supported with this model" and "model does not support tool use".
    let mentions_tools = l.contains("tool calling")
        || l.contains("tool use")
        || l.contains("tool_use")
        || l.contains("tool calls")
        || l.contains("function calling")
        || l.contains("function-calling")
        || l.contains("function call");
    let unsupported = l.contains("not supported")
        || l.contains("does not support")
        || l.contains("isn't supported")
        || l.contains("unsupported");
    mentions_tools && unsupported
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
            if let Some(secs) = parse_secs(&lower[idx + marker.len()..]) {
                return Some(std::time::Duration::from_secs_f64(secs));
            }
        }
    }
    None
}

/// Pull the first floating-point number out of `s` (skipping leading quotes/colons/spaces),
/// e.g. `": \"37.04s\""` → `37.04`. Stops at the first non-numeric char after digits.
fn parse_secs(s: &str) -> Option<f64> {
    let mut num = String::new();
    let mut started = false;
    for c in s.chars() {
        if c.is_ascii_digit() || (c == '.' && started) {
            num.push(c);
            started = true;
        } else if started {
            break;
        } else if c == '"' || c == ':' || c == ' ' || c == '=' {
            continue;
        } else {
            // a non-numeric, non-separator char before any digit — give up.
            return None;
        }
    }
    num.parse::<f64>().ok()
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
        // Apply the caller's reasoning-effort hint when set (e.g. from `/effort high`).
        if let Some(effort) = opts.effort {
            let re = match effort {
                EffortLevel::Low => ReasoningEffort::Low,
                EffortLevel::Medium => ReasoningEffort::Medium,
                EffortLevel::High => ReasoningEffort::High,
                EffortLevel::XHigh => ReasoningEffort::XHigh,
            };
            options = options.with_reasoning_effort(re);
        }

        // Stall guards: a hung connection or a stream that goes silent must not freeze the
        // turn forever. A timeout surfaces as `Unavailable` (retryable), so the mesh fails over
        // to the next model instead of spinning indefinitely (model-health-failover).
        let res = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.client
                .exec_chat_stream(model_name.as_str(), req, Some(&options)),
        )
        .await
        .map_err(|_| stall_error("no response while connecting", CONNECT_TIMEOUT))?
        .map_err(|e| classify_genai_error(&e))?;

        let mut stream = res.stream;
        let mut content = String::new();
        let mut usage = Usage::default();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

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
                                args: tc.fn_arguments,
                            })
                            .collect();
                    }
                }
                _ => {}
            }
        }

        Ok(ModelResponse {
            content,
            tool_calls,
            usage,
            quotas: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
