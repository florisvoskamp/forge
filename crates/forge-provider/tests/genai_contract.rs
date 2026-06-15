//! Layer 2 — HTTP contract tests for `GenAiProvider` (FR-3). They point genai at a local
//! `httpmock` server via a service-target resolver (loopback endpoint + dummy auth), so the
//! *real* genai adapter builds and parses real HTTP/SSE — but no API key is used and no byte
//! leaves the machine. This exercises the streaming/usage/tool-call branches that the
//! `MockProvider` tests cannot reach.

use forge_provider::{GenAiProvider, Provider, ProviderError, StreamEvent, ToolSpec};
use forge_types::Message;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ServiceTarget};
use httpmock::prelude::*;
use serde_json::json;

/// Build a genai client whose every request is redirected to `base` with a throwaway key.
fn client_pointed_at(base: String) -> Client {
    Client::builder()
        .with_service_target_resolver_fn(move |mut t: ServiceTarget| {
            t.endpoint = Endpoint::from_owned(format!("{base}/"));
            t.auth = AuthData::from_single("test-key");
            Ok::<ServiceTarget, genai::resolver::Error>(t)
        })
        .build()
}

const SSE_CT: &str = "text/event-stream";

#[tokio::test]
async fn streaming_accumulates_deltas_and_usage() {
    let server = MockServer::start_async().await;
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    let _m = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200).header("content-type", SSE_CT).body(body);
    });

    let provider = GenAiProvider::with_client(client_pointed_at(server.base_url()));
    let mut sink = String::new();
    let res = provider
        .complete(
            "openai::gpt-4o-mini",
            &[Message::user("hi")],
            &[],
            &mut |ev| {
                if let StreamEvent::Text(t) = ev {
                    sink.push_str(&t)
                }
            },
        )
        .await
        .expect("complete should succeed against the mock");

    assert_eq!(sink, "Hello", "deltas streamed to the sink in order");
    assert_eq!(res.content, "Hello", "content is the concatenation");
    assert_eq!(res.usage.input_tokens, 5);
    assert_eq!(res.usage.output_tokens, 2);
    assert!(res.tool_calls.is_empty());
}

#[tokio::test]
async fn tool_call_is_translated() {
    let server = MockServer::start_async().await;
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let _m = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200).header("content-type", SSE_CT).body(body);
    });

    let tools = [ToolSpec {
        name: "read_file".into(),
        description: "read".into(),
        schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
    }];
    let provider = GenAiProvider::with_client(client_pointed_at(server.base_url()));
    let res = provider
        .complete(
            "openai::gpt-4o-mini",
            &[Message::user("read it")],
            &tools,
            &mut |_| {},
        )
        .await
        .expect("complete should succeed");

    assert!(res.wants_tools(), "a tool call was requested");
    assert_eq!(res.tool_calls.len(), 1);
    let call = &res.tool_calls[0];
    assert_eq!(call.id, "call_abc");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.args["path"], "Cargo.toml");
}

#[tokio::test]
async fn http_500_maps_to_unavailable_for_failover() {
    // A 5xx is a transient provider problem → classified Unavailable (retryable) so the mesh
    // benches the model and fails over (model-health-failover), not a hard Request failure.
    let server = MockServer::start_async().await;
    let _m = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(500).body("boom");
    });

    let provider = GenAiProvider::with_client(client_pointed_at(server.base_url()));
    let err = provider
        .complete(
            "openai::gpt-4o-mini",
            &[Message::user("hi")],
            &[],
            &mut |_| {},
        )
        .await
        .expect_err("a 500 must surface as an error");
    assert!(err.is_retryable(), "5xx should be retryable: {err:?}");
    assert!(matches!(err, ProviderError::Unavailable(_)), "got {err:?}");
}
