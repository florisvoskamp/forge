//! `genai`-backed implementation of [`Provider`] (ADR-0003). genai 0.6 resolves an adapter
//! per `namespace::model` id, so this one backend covers Anthropic, OpenAI, Gemini, xAI,
//! DeepSeek, OpenRouter, Groq, OpenCode Zen (`opencode_go`), GitHub Models, MiMo, MiniMax,
//! Ollama, … plus Cerebras via a custom-endpoint resolver ([`build_client`]). Tool calling is
//! normalized: tools are advertised, the model's calls map back to Forge [`ToolCall`]s, and
//! prior tool results are replayed as genai tool responses so multi-step loops round-trip.

use async_trait::async_trait;
use forge_types::{Message, Role, ToolCall, Usage};
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, Tool, ToolCall as GenAiToolCall,
    ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

use crate::{EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec};

#[derive(Default)]
pub struct GenAiProvider {
    client: Client,
}

impl GenAiProvider {
    pub fn new() -> Self {
        Self {
            client: build_client(),
        }
    }

    /// Construct with a caller-supplied `genai::Client`. Used by the HTTP contract tests to
    /// point genai at a local mock server; otherwise identical to [`GenAiProvider::new`].
    pub fn with_client(client: Client) -> Self {
        Self { client }
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
fn build_client() -> Client {
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
fn to_genai_model(model: &str) -> String {
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
            Role::User => out.push(ChatMessage::user(m.content.clone())),
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

fn to_genai_tool(spec: &ToolSpec) -> Tool {
    Tool::new(spec.name.clone())
        .with_description(spec.description.clone())
        .with_schema(spec.schema.clone())
}

/// Map a `genai::Error` to a classified [`ProviderError`] so the mesh can decide whether to
/// bench the model + fail over (429 / 5xx / auth) or fail the turn (everything else). Uses the
/// typed `StatusCode`/`HeaderMap` where genai exposes them (`HttpError`, `WebModelCall`); for
/// the *streaming* path Forge uses, genai only carries a string, so we scan it.
fn classify_genai_error(err: &genai::Error) -> ProviderError {
    use genai::webc::Error as WebcError;
    match err {
        genai::Error::HttpError { status, body, .. } => classify_status(
            status.as_u16(),
            err.to_string(),
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
                classify_status(status.as_u16(), err.to_string(), retry_after)
            }
            other => ProviderError::Unavailable(other.to_string()),
        },
        // Streaming path: status lives only in the message string (`...Status: 429...`).
        genai::Error::WebStream { cause, .. } => classify_text(cause, err.to_string()),
        genai::Error::ChatResponse { body, .. } => {
            classify_text(&body.to_string(), err.to_string())
        }
        // A bad/truncated stream chunk — transient, worth trying elsewhere.
        genai::Error::StreamParse { .. } => ProviderError::Unavailable(err.to_string()),
        other => ProviderError::Request(other.to_string()),
    }
}

/// Classify from an HTTP status code.
fn classify_status(
    code: u16,
    message: String,
    retry_after: Option<std::time::Duration>,
) -> ProviderError {
    match code {
        429 => ProviderError::RateLimited {
            message,
            retry_after,
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
    if has("429") || has("resource_exhausted") || has("rate limit") || has("quota") {
        ProviderError::RateLimited {
            message,
            retry_after: parse_retry_after_body(text),
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
        let model_name = to_genai_model(model);

        let mut req = ChatRequest::new(to_genai_messages(messages));
        if !tools.is_empty() {
            req = req.with_tools(tools.iter().map(to_genai_tool).collect::<Vec<_>>());
        }

        // Capture flags so the terminal End event carries usage + tool calls.
        let options = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true);

        let res = self
            .client
            .exec_chat_stream(model_name.as_str(), req, Some(&options))
            .await
            .map_err(|e| classify_genai_error(&e))?;

        let mut stream = res.stream;
        let mut content = String::new();
        let mut usage = Usage::default();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(event) = stream.next().await {
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
                        usage = Usage {
                            input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u64,
                            output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u64,
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn classify_text_429_is_rate_limited_with_server_cooldown() {
        let e = classify_text(GEMINI_429, "stream err".into());
        match e {
            ProviderError::RateLimited { retry_after, .. } => {
                // `retryDelay":"37s"` is matched before the looser "retry in 37.04s".
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(37)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert!(e.is_retryable());
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
    fn classify_status_maps_codes() {
        let none = None;
        assert!(matches!(
            classify_status(429, "x".into(), none),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_status(401, "x".into(), None),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            classify_status(503, "x".into(), None),
            ProviderError::Unavailable(_)
        ));
        // 400 misuse is non-retryable — must not fail over.
        let bad = classify_status(400, "x".into(), None);
        assert!(matches!(bad, ProviderError::Request(_)));
        assert!(!bad.is_retryable());
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
