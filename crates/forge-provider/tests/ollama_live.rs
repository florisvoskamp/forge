//! Layer 3 — local integration against a real Ollama. Off the default/CI path: every test
//! is `#[ignore]`d AND early-returns unless `FORGE_OLLAMA_TESTS=1`. Run on a box with Ollama:
//!
//!   FORGE_OLLAMA_TESTS=1 cargo test -p forge-provider --test ollama_live -- --ignored

use forge_provider::{GenAiProvider, Provider, ToolSpec};
use forge_types::Message;
use serde_json::json;

fn enabled() -> bool {
    std::env::var("FORGE_OLLAMA_TESTS").is_ok()
}

#[tokio::test]
#[ignore = "requires local Ollama; run with FORGE_OLLAMA_TESTS=1 -- --ignored"]
async fn ollama_round_trip_returns_text() {
    if !enabled() {
        return;
    }
    let provider = GenAiProvider::new();
    let res = provider
        .complete(
            "ollama::llama3.2",
            &[Message::user("Reply with the single word: hi")],
            &[],
            &mut |_| {},
        )
        .await
        .expect("a real Ollama turn should complete");
    assert!(!res.content.is_empty(), "Ollama returned some text");
}

#[tokio::test]
#[ignore = "requires local Ollama; run with FORGE_OLLAMA_TESTS=1 -- --ignored"]
async fn ollama_tool_advertised_yields_call_or_text() {
    if !enabled() {
        return;
    }
    let tools = [ToolSpec {
        name: "read_file".into(),
        description: "Read a file at the given path.".into(),
        schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }];
    let provider = GenAiProvider::new();
    let res = provider
        .complete(
            "ollama::llama3.2",
            &[Message::user("Read the file Cargo.toml")],
            &tools,
            &mut |_| {},
        )
        .await
        .expect("a real Ollama turn should complete");
    // Small local models are unreliable at tool use; assert capability, not determinism.
    assert!(
        res.wants_tools() || !res.content.is_empty(),
        "either a tool call or some text came back"
    );
}
