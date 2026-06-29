//! Fetch per-model context windows from provider model APIs and persist them to the store, so the
//! core can trim each turn's transcript to fit the routed model's window. Without this, a long
//! conversation overflows a free model's (often 32k–128k) window and the request fails — which the
//! mesh sees as the model being "unavailable", cascading through the whole fallback chain.
//!
//! Which providers expose context info in their model-listing API:
//!   ✅ Anthropic   GET /v1/models               → `context_window` per model
//!   ✅ Gemini      GET /v1beta/models            → `inputTokenLimit` per model
//!   ✅ Groq        GET /openai/v1/models         → `context_window` per model
//!   ❌ OpenAI      GET /v1/models               — no context field
//!   ❌ NVIDIA NIM  GET /v1/models               — no context field
//!   ❌ xAI                                      — no context field
//!   ❌ DeepSeek                                 — no models-listing API
//!   ❌ Mistral                                  — no context field
//!   ? Custom/Cerebras/SambaNova                — best-effort; field present on some
//!
//! For providers without native context info we use two fallback strategies:
//! 1. OpenRouter cross-map: OR lists each model with a `context_length`. We derive native Forge
//!    IDs from `vendor/model` → `ns::model` using a vendor prefix table.
//! 2. OR basename lookup: for custom providers (e.g. NVIDIA NIM) that return multi-vendor model
//!    IDs like `meta/llama-3.1-405b-instruct`, we extract the basename `llama-3.1-405b-instruct`
//!    and look it up in OR's index. This covers NVIDIA NIM's full model catalog even though OR
//!    lists those models under `meta-llama/` rather than `nvidia/`.

use std::collections::HashMap;
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetch per-model context windows AND prices from all reachable provider APIs, persisting
/// results into the DB. Best-effort and fail-soft — any error just leaves the conservative floor.
pub async fn fetch_and_persist(models: &[String]) {
    let Ok(store) = crate::open_store() else {
        return;
    };
    let wanted: std::collections::HashSet<&str> = models.iter().map(String::as_str).collect();

    // ── OpenRouter first (keyless, always) ───────────────────────────────────────────────────────
    // Fetched before native providers so we can build the basename fallback index used by custom
    // provider fetches below. Native fetches (Anthropic, Gemini, Groq) run afterward and overwrite
    // OR-derived values with authoritative data where available.
    let or_basename_index =
        if let Some(body) = get_json("https://openrouter.ai/api/v1/models", None).await {
            // openrouter:: windows
            for (id, w) in openrouter_windows(&body) {
                if wanted.contains(id.as_str()) {
                    let _ = store.set_model_context(&id, w);
                }
            }
            // cross-map to native namespaces (openai::, xai::, deepseek::, mistral::, nvidia::, …)
            for (id, w) in openrouter_native_cross_map(&body) {
                let _ = store.set_model_context(&id, w);
            }
            // pricing
            for (id, in_1k, out_1k, cache_1k) in openrouter_pricing(&body) {
                let _ = store.set_model_pricing(&id, in_1k, out_1k, cache_1k);
            }
            // basename index: "llama-3.1-405b-instruct" → 131072
            // Used below as fallback for custom providers that don't include context info in /v1/models
            // but whose models appear on OR under a different vendor prefix (e.g. NVIDIA NIM hosts
            // `meta/llama-3.1-405b-instruct`; OR lists it as `meta-llama/llama-3.1-405b-instruct`).
            build_basename_index(&body)
        } else {
            HashMap::new()
        };

    // ── Custom OpenAI-compatible providers (NVIDIA NIM, Cerebras, SambaNova, …) ─────────────────
    // Fetched before the other native providers so the Anthropic/Gemini/Groq authoritative writes
    // happen last and win over any cross-provider basename matches.
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
            let native = openai_compatible_windows(&body, ns);
            let native_ids: std::collections::HashSet<&str> =
                native.iter().map(|(id, _)| id.as_str()).collect();

            // Persist whatever the native endpoint returned.
            for (id, w) in &native {
                let _ = store.set_model_context(id, *w);
            }

            // For models the native endpoint listed but gave no context for, fall back to OR
            // basename index. E.g. NVIDIA NIM returns `meta/llama-3.1-405b-instruct` without a
            // context_length field; basename `llama-3.1-405b-instruct` matches OR's 131072.
            if !or_basename_index.is_empty() {
                if let Some(data) = body["data"].as_array() {
                    for m in data {
                        if let Some(id) = m["id"].as_str() {
                            let forge_id = format!("{ns}::{id}");
                            if native_ids.contains(forge_id.as_str()) {
                                continue; // already has context from native
                            }
                            let basename = id.split('/').next_back().unwrap_or(id);
                            if let Some(&w) = or_basename_index.get(basename) {
                                let _ = store.set_model_context(&forge_id, w);
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Anthropic native (authoritative) ─────────────────────────────────────────────────────────
    if models
        .iter()
        .any(|m| forge_config::provider_of(m) == "anthropic")
    {
        if let Ok(key) = forge_config::api_key("anthropic") {
            if let Some(body) = get_json_with_headers(
                "https://api.anthropic.com/v1/models",
                &[
                    ("x-api-key", key.as_str()),
                    ("anthropic-version", "2023-06-01"),
                ],
            )
            .await
            {
                for (id, w) in anthropic_windows(&body) {
                    let _ = store.set_model_context(&id, w);
                }
            }
        }
    }

    // ── Gemini native (authoritative) ─────────────────────────────────────────────────────────────
    if models
        .iter()
        .any(|m| forge_config::provider_of(m) == "gemini")
    {
        if let Ok(key) = forge_config::api_key("gemini") {
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models?key={key}&pageSize=100"
            );
            if let Some(body) = get_json(&url, None).await {
                for (id, w) in gemini_windows(&body) {
                    let _ = store.set_model_context(&id, w);
                }
            }
        }
    }

    // ── Groq (authoritative) ─────────────────────────────────────────────────────────────────────
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
}

// ── parsers ──────────────────────────────────────────────────────────────────────────────────────

/// Build `basename → context_window` index from OR data for use as a cross-provider fallback.
/// E.g. `"meta-llama/llama-3.1-405b-instruct"` → key `"llama-3.1-405b-instruct"` → 131072.
/// When multiple OR models share a basename the largest window wins (conservatively).
fn build_basename_index(body: &serde_json::Value) -> HashMap<String, u32> {
    let Some(data) = body["data"].as_array() else {
        return HashMap::new();
    };
    let mut map: HashMap<String, u32> = HashMap::new();
    for m in data {
        let Some(id) = m["id"].as_str() else {
            continue;
        };
        let Some(window) = m["context_length"].as_u64().filter(|w| *w > 0) else {
            continue;
        };
        let basename = id.split('/').next_back().unwrap_or(id);
        let w = window.min(u32::MAX as u64) as u32;
        map.entry(basename.to_string())
            .and_modify(|e| *e = (*e).max(w))
            .or_insert(w);
    }
    map
}

/// Extract `(anthropic::<id>, window)` from Anthropic's `/v1/models` response.
fn anthropic_windows(body: &serde_json::Value) -> Vec<(String, u32)> {
    let Some(data) = body["data"].as_array() else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let window = m["context_window"].as_u64().filter(|w| *w > 0)?;
            Some((
                format!("anthropic::{id}"),
                window.min(u32::MAX as u64) as u32,
            ))
        })
        .collect()
}

/// Extract `(gemini::<id>, window)` from Google's `/v1beta/models` response.
fn gemini_windows(body: &serde_json::Value) -> Vec<(String, u32)> {
    let Some(models) = body["models"].as_array() else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            let model_id = name.strip_prefix("models/").unwrap_or(name);
            let window = m["inputTokenLimit"].as_u64().filter(|w| *w > 0)?;
            Some((
                format!("gemini::{model_id}"),
                window.min(u32::MAX as u64) as u32,
            ))
        })
        .collect()
}

/// Extract `(<namespace>::<id>, window)` from an OpenAI-compatible `/v1/models` body.
/// Tries `context_window` (Groq) then `context_length` (some custom providers).
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

/// Extract `(openrouter::<id>, window)` pairs from OR's `/api/v1/models` body.
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

/// Cross-map OpenRouter model IDs to native Forge provider namespaces.
///
/// `strip_prefix` controls whether the vendor prefix is stripped from the model part:
/// - `true`  (e.g. anthropic): `anthropic/claude-opus-4-8` → `anthropic::claude-opus-4-8`
/// - `false` (e.g. nvidia): `nvidia/llama-3.1-nemotron-70b-instruct`
///   → `nvidia::nvidia/llama-3.1-nemotron-70b-instruct`
///   NVIDIA NIM returns model IDs with their vendor prefix (`nvidia/model`), so keeping the
///   full path as the model part matches the Forge catalog ID.
fn openrouter_native_cross_map(body: &serde_json::Value) -> Vec<(String, u32)> {
    // (or_vendor_prefix, forge_namespace, strip_prefix)
    const VENDOR_MAP: &[(&str, &str, bool)] = &[
        ("anthropic/", "anthropic", true),
        ("google/", "gemini", true),
        ("openai/", "openai", true),
        ("x-ai/", "xai", true),
        ("deepseek/", "deepseek", true),
        ("mistralai/", "mistral", true),
        ("cohere/", "cohere", true),
        ("amazon/", "amazon", true),
        // NVIDIA: keep full path so "nvidia/model" → "nvidia::nvidia/model"
        ("nvidia/", "nvidia", false),
    ];

    let Some(data) = body["data"].as_array() else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| {
            let or_id = m["id"].as_str()?;
            let window = m["context_length"].as_u64().filter(|w| *w > 0)?;
            let (ns, model_id) = VENDOR_MAP.iter().find_map(|(prefix, ns, strip)| {
                if or_id.starts_with(prefix) {
                    let model_id = if *strip {
                        or_id.strip_prefix(prefix).unwrap_or(or_id)
                    } else {
                        or_id
                    };
                    Some((*ns, model_id))
                } else {
                    None
                }
            })?;
            Some((
                format!("{ns}::{model_id}"),
                window.min(u32::MAX as u64) as u32,
            ))
        })
        .collect()
}

/// Extract pricing from OR's `/api/v1/models` body.
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

// ── HTTP helpers ─────────────────────────────────────────────────────────────────────────────────

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

async fn get_json_with_headers(url: &str, headers: &[(&str, &str)]) -> Option<serde_json::Value> {
    let mut req = forge_provider::bundled_http_client()
        .get(url)
        .timeout(FETCH_TIMEOUT);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!("models endpoint {} returned {}", url, resp.status());
        return None;
    }
    resp.json().await.ok()
}

// ── tests ─────────────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn or_body() -> serde_json::Value {
        json!({
            "data": [
                { "id": "anthropic/claude-opus-4-8", "context_length": 200000,
                  "pricing": { "prompt": "0.000015", "completion": "0.000075" } },
                { "id": "google/gemini-2.5-pro", "context_length": 1048576,
                  "pricing": { "prompt": "0.00000125", "completion": "0.00001" } },
                { "id": "openai/gpt-4o", "context_length": 128000,
                  "pricing": { "prompt": "0.0000025", "completion": "0.00001" } },
                { "id": "x-ai/grok-4", "context_length": 131072,
                  "pricing": { "prompt": "0.000003", "completion": "0.000015" } },
                { "id": "deepseek/deepseek-chat", "context_length": 65536,
                  "pricing": { "prompt": "0.00000027", "completion": "0.0000011" } },
                { "id": "mistralai/mistral-large", "context_length": 131072,
                  "pricing": { "prompt": "0.000002", "completion": "0.000006" } },
                { "id": "nvidia/llama-3.1-nemotron-70b-instruct", "context_length": 131072,
                  "pricing": { "prompt": "0.00000035", "completion": "0.0000004" } },
                // meta-llama/ — used by basename fallback for nvidia::meta/llama-3.1-405b-instruct
                { "id": "meta-llama/llama-3.1-405b-instruct", "context_length": 131072,
                  "pricing": { "prompt": "0.0000008", "completion": "0.0000008" } },
                // unmapped vendor
                { "id": "vendor/paid", "context_length": 131072,
                  "pricing": { "prompt": "0.0000002", "completion": "0.0000008" } },
            ]
        })
    }

    #[test]
    fn openrouter_windows_keyed_by_or_id() {
        let windows = openrouter_windows(&or_body());
        assert!(windows.contains(&("openrouter::vendor/paid".to_string(), 131072)));
        assert!(windows.contains(&("openrouter::openai/gpt-4o".to_string(), 128000)));
    }

    #[test]
    fn cross_maps_to_native_namespaces() {
        let mapped = openrouter_native_cross_map(&or_body());
        assert!(mapped.contains(&("anthropic::claude-opus-4-8".to_string(), 200000)));
        assert!(mapped.contains(&("gemini::gemini-2.5-pro".to_string(), 1048576)));
        assert!(mapped.contains(&("openai::gpt-4o".to_string(), 128000)));
        assert!(mapped.contains(&("xai::grok-4".to_string(), 131072)));
        assert!(mapped.contains(&("deepseek::deepseek-chat".to_string(), 65536)));
        assert!(mapped.contains(&("mistral::mistral-large".to_string(), 131072)));
        // nvidia: strip_prefix=false → keeps full id
        assert!(mapped.contains(&(
            "nvidia::nvidia/llama-3.1-nemotron-70b-instruct".to_string(),
            131072
        )));
        // unmapped vendor → not present
        assert!(!mapped.iter().any(|(id, _)| id.starts_with("meta-llama::")));
    }

    #[test]
    fn basename_index_covers_nvidia_meta_models() {
        let index = build_basename_index(&or_body());
        // "meta-llama/llama-3.1-405b-instruct" → basename "llama-3.1-405b-instruct" → 131072
        assert_eq!(index.get("llama-3.1-405b-instruct"), Some(&131072));
        // "nvidia/llama-3.1-nemotron-70b-instruct" → basename "llama-3.1-nemotron-70b-instruct"
        assert_eq!(index.get("llama-3.1-nemotron-70b-instruct"), Some(&131072));
        assert_eq!(index.get("gemini-2.5-pro"), Some(&1048576));
    }

    #[test]
    fn basename_index_largest_window_wins_on_conflict() {
        let body = json!({
            "data": [
                { "id": "vendor-a/some-model", "context_length": 32768 },
                { "id": "vendor-b/some-model", "context_length": 131072 },
            ]
        });
        let index = build_basename_index(&body);
        assert_eq!(index.get("some-model"), Some(&131072));
    }

    #[test]
    fn parses_anthropic_context_window_field() {
        let body = json!({
            "data": [
                { "id": "claude-opus-4-8", "context_window": 200000 },
                { "id": "claude-haiku-4-5", "context_window": 200000 },
            ]
        });
        let windows = anthropic_windows(&body);
        assert!(windows.contains(&("anthropic::claude-opus-4-8".to_string(), 200000)));
        assert!(windows.contains(&("anthropic::claude-haiku-4-5".to_string(), 200000)));
    }

    #[test]
    fn parses_gemini_input_token_limit() {
        let body = json!({
            "models": [
                { "name": "models/gemini-2.5-pro", "inputTokenLimit": 1048576 },
                { "name": "models/gemini-2.5-flash", "inputTokenLimit": 1048576 },
                { "name": "models/text-embedding-004" },
            ]
        });
        let windows = gemini_windows(&body);
        assert!(windows.contains(&("gemini::gemini-2.5-pro".to_string(), 1048576)));
        assert!(windows.contains(&("gemini::gemini-2.5-flash".to_string(), 1048576)));
        assert!(!windows.iter().any(|(id, _)| id.contains("embedding")));
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
    fn openrouter_pricing_parses_correctly() {
        let prices = openrouter_pricing(&or_body());
        let opus = prices
            .iter()
            .find(|(id, ..)| id == "openrouter::anthropic/claude-opus-4-8")
            .unwrap();
        // "0.000015" per-token * 1000 = 0.015 per-1k
        assert!((opus.1 - 0.015).abs() < 1e-9);
        assert!((opus.2 - 0.075).abs() < 1e-9);
    }
}
