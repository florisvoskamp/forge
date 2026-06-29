//! Fetch per-model context windows from provider model APIs and persist them to the store, so the
//! core can trim each turn's transcript to fit the routed model's window. Without this, a long
//! conversation overflows a free model's (often 32k–128k) window and the request fails — which the
//! mesh sees as the model being "unavailable", cascading through the whole fallback chain.
//!
//! Sources (all best-effort, fail-soft):
//! - OpenRouter   `/api/v1/models`       — keyless, `context_length` field
//! - Groq         `/openai/v1/models`    — keyed,   `context_window` field
//! - Custom OpenAI-compatible providers  — keyed,   `context_length` or `context_window` field

use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetch per-model context windows AND prices from all reachable provider APIs, persisting the
/// results for any model present in `models`. Best-effort and fail-soft: a network/parse error
/// just leaves the conservative floor in charge for that provider. Skips a provider entirely when
/// no model from it is in the current catalog.
pub async fn fetch_and_persist(models: &[String]) {
    let Ok(store) = crate::open_store() else {
        return;
    };
    let wanted: std::collections::HashSet<&str> = models.iter().map(String::as_str).collect();

    // ── OpenRouter (keyless) ──────────────────────────────────────────────────────────────────────
    if models
        .iter()
        .any(|m| forge_config::provider_of(m) == "openrouter")
    {
        if let Some(body) = get_json("https://openrouter.ai/api/v1/models", None).await {
            for (id, w) in openrouter_windows(&body) {
                if wanted.contains(id.as_str()) {
                    let _ = store.set_model_context(&id, w);
                }
            }
            // Persist pricing for every model returned, not just the catalog subset.
            for (id, in_1k, out_1k, cache_1k) in openrouter_pricing(&body) {
                let _ = store.set_model_pricing(&id, in_1k, out_1k, cache_1k);
            }
        }
    }

    // ── Groq ─────────────────────────────────────────────────────────────────────────────────────
    if models
        .iter()
        .any(|m| forge_config::provider_of(m) == "groq")
    {
        if let Ok(key) = forge_config::api_key("groq") {
            if let Some(body) = get_json("https://api.groq.com/openai/v1/models", Some(&key)).await
            {
                for (id, w) in openai_compatible_windows(&body, "groq") {
                    let _ = store.set_model_context(&id, w);
                }
            }
        }
    }

    // ── Custom OpenAI-compatible providers (NVIDIA NIM, Cerebras, SambaNova, Mistral, …) ─────────
    for cp in forge_config::custom_providers() {
        let ns = cp.namespace;
        if !models.iter().any(|m| forge_config::provider_of(m) == ns) {
            continue;
        }
        let Ok(key) = forge_config::api_key(ns) else {
            continue;
        };
        let url = format!("{}models", cp.endpoint);
        if let Some(body) = get_json(&url, Some(&key)).await {
            for (id, w) in openai_compatible_windows(&body, ns) {
                let _ = store.set_model_context(&id, w);
            }
        }
    }
}

/// Extract `(openrouter::<id>, window)` pairs from the `/api/v1/models` body
/// (`{ "data": [ { "id": …, "context_length": 131072 } ] }`).
fn openrouter_windows(body: &serde_json::Value) -> Vec<(String, u32)> {
    let Some(data) = body["data"].as_array() else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let window = m["context_length"].as_u64().filter(|w| *w > 0)?;
            Some((
                format!("openrouter::{id}"),
                window.min(u32::MAX as u64) as u32,
            ))
        })
        .collect()
}

/// Extract `(<namespace>::<id>, window)` pairs from an OpenAI-compatible `/v1/models` body.
/// Tries `context_window` (Groq) then `context_length` (NVIDIA NIM, others).
fn openai_compatible_windows(body: &serde_json::Value, namespace: &str) -> Vec<(String, u32)> {
    let Some(data) = body["data"].as_array() else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let window = m["context_window"]
                .as_u64()
                .or_else(|| m["context_length"].as_u64())
                .filter(|w| *w > 0)?;
            Some((
                format!("{namespace}::{id}"),
                window.min(u32::MAX as u64) as u32,
            ))
        })
        .collect()
}

/// Extract `(openrouter::<id>, input_per_1k, output_per_1k, cache_read_per_1k)` from the body.
/// OpenRouter quotes `pricing.prompt` / `pricing.completion` / `pricing.input_cache_read` as
/// USD-per-token strings (e.g. "0.0000002"), so multiply by 1000 for per-1k. A `$0` (free) model is
/// kept — recording 0.0 is correct and stops the price being re-fetched as "unknown". The cache-read
/// rate is optional (many models omit it). Models with no usable prompt/completion block are skipped.
fn openrouter_pricing(body: &serde_json::Value) -> Vec<(String, f64, f64, Option<f64>)> {
    let Some(data) = body["data"].as_array() else {
        return Vec::new();
    };
    let per_1k = |v: &serde_json::Value| -> Option<f64> {
        let n = v
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| v.as_f64())?;
        (n.is_finite() && n >= 0.0).then_some(n * 1000.0)
    };
    data.iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let pricing = &m["pricing"];
            let input = per_1k(&pricing["prompt"])?;
            let output = per_1k(&pricing["completion"])?;
            let cache_read = per_1k(&pricing["input_cache_read"]);
            Some((format!("openrouter::{id}"), input, output, cache_read))
        })
        .collect()
}

async fn get_json(url: &str, bearer: Option<&str>) -> Option<serde_json::Value> {
    let mut req = forge_provider::bundled_http_client()
        .get(url)
        .timeout(FETCH_TIMEOUT);
    if let Some(key) = bearer {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!("models endpoint {} returned {}", url, resp.status());
        return None;
    }
    resp.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_openrouter_windows_and_pricing() {
        let body = json!({
            "data": [
                { "id": "vendor/paid", "context_length": 131072,
                  "pricing": { "prompt": "0.0000002", "completion": "0.0000008",
                               "input_cache_read": "0.00000005" } },
                { "id": "vendor/free", "context_length": 32768,
                  "pricing": { "prompt": "0", "completion": "0" } },
                { "id": "vendor/nopricing", "context_length": 8192 },
            ]
        });
        let windows = openrouter_windows(&body);
        assert!(windows.contains(&("openrouter::vendor/paid".to_string(), 131072)));

        let prices = openrouter_pricing(&body);
        let paid = prices
            .iter()
            .find(|(id, ..)| id == "openrouter::vendor/paid")
            .unwrap();
        assert!((paid.1 - 0.0002).abs() < 1e-12);
        assert!((paid.2 - 0.0008).abs() < 1e-12);
        assert!((paid.3.unwrap() - 0.00005).abs() < 1e-12);
        assert!(prices
            .iter()
            .any(|(id, i, o, c)| id == "openrouter::vendor/free"
                && *i == 0.0
                && *o == 0.0
                && c.is_none()));
        assert!(!prices
            .iter()
            .any(|(id, ..)| id == "openrouter::vendor/nopricing"));
    }

    #[test]
    fn parses_groq_context_window_field() {
        let body = json!({
            "data": [
                { "id": "llama-3.3-70b-versatile", "context_window": 131072 },
                { "id": "gemma2-9b-it", "context_window": 8192 },
            ]
        });
        let windows = openai_compatible_windows(&body, "groq");
        assert!(windows.contains(&("groq::llama-3.3-70b-versatile".to_string(), 131072)));
        assert!(windows.contains(&("groq::gemma2-9b-it".to_string(), 8192)));
    }

    #[test]
    fn parses_nvidia_context_length_field() {
        let body = json!({
            "data": [
                { "id": "meta/llama-3.1-405b-instruct", "context_length": 131072 },
                { "id": "nvidia/llama-3.1-nemotron-70b-instruct" },
            ]
        });
        let windows = openai_compatible_windows(&body, "nvidia");
        assert!(windows.contains(&("nvidia::meta/llama-3.1-405b-instruct".to_string(), 131072)));
        // No context field → excluded (not persisted)
        assert!(!windows
            .iter()
            .any(|(id, _)| id == "nvidia::nvidia/llama-3.1-nemotron-70b-instruct"));
    }
}
