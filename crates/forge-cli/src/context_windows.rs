//! Fetch per-model context windows from provider model APIs and persist them to the store, so the
//! core can trim each turn's transcript to fit the routed model's window. Without this, a long
//! conversation overflows a free model's (often 32k–128k) window and the request fails — which the
//! mesh sees as the model being "unavailable", cascading through the whole fallback chain.
//!
//! Today only OpenRouter exposes a per-model `context_length` in a key-free list endpoint. Other
//! providers fall back to forge-mesh's family heuristic (`pricing::context_limit`), then a floor.

use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetch OpenRouter per-model context windows AND prices, persisting the ones present in `models`.
/// Best-effort and fail-soft: a network/parse error just leaves the heuristics in charge. Skips the
/// network entirely when no OpenRouter model is in the catalog. The price persistence matters for
/// the budget cap: most gateway models aren't in the bundled rate table, so without a fetched price
/// their spend computes to $0 and the cap can't see it.
pub async fn fetch_and_persist(models: &[String]) {
    if !models
        .iter()
        .any(|m| forge_config::provider_of(m) == "openrouter")
    {
        return;
    }
    let Some(body) = get_json("https://openrouter.ai/api/v1/models").await else {
        return;
    };
    let windows = openrouter_windows(&body);
    let prices = openrouter_pricing(&body);
    if windows.is_empty() && prices.is_empty() {
        return;
    }
    let Ok(store) = crate::open_store() else {
        return;
    };
    let wanted: std::collections::HashSet<&str> = models.iter().map(String::as_str).collect();
    for (id, w) in windows {
        if wanted.contains(id.as_str()) {
            let _ = store.set_model_context(&id, w);
        }
    }
    // Persist pricing for EVERY model the endpoint returns, not just the currently-discovered
    // subset: the mesh may fail over to, or later discover, a model that wasn't in `wanted` at this
    // moment (or the balance pre-filter dropped it). An unpriced routed model computes to $0 and
    // silently breaks cost tracking, so over-storing a few hundred tiny rows is the safe trade.
    for (id, in_1k, out_1k, cache_1k) in prices {
        let _ = store.set_model_pricing(&id, in_1k, out_1k, cache_1k);
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
        // Field is a string; tolerate a numeric too. Reject negatives / NaN.
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

async fn get_json(url: &str) -> Option<serde_json::Value> {
    let resp = forge_provider::bundled_http_client()
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!("openrouter models endpoint returned {}", resp.status());
        return None;
    }
    resp.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_windows_and_per_token_pricing() {
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
        // Per-token strings → per-1k (×1000): 0.0000002 → 0.0002, 0.0000008 → 0.0008.
        let paid = prices
            .iter()
            .find(|(id, ..)| id == "openrouter::vendor/paid")
            .unwrap();
        assert!((paid.1 - 0.0002).abs() < 1e-12);
        assert!((paid.2 - 0.0008).abs() < 1e-12);
        // Cache-read rate parsed: 0.00000005 → 0.00005 per 1k.
        assert!((paid.3.unwrap() - 0.00005).abs() < 1e-12);
        // Free model is recorded as 0.0 (kept, not skipped); no cache rate → None.
        assert!(prices
            .iter()
            .any(|(id, i, o, c)| id == "openrouter::vendor/free"
                && *i == 0.0
                && *o == 0.0
                && c.is_none()));
        // No pricing block → skipped entirely.
        assert!(!prices
            .iter()
            .any(|(id, ..)| id == "openrouter::vendor/nopricing"));
    }
}
