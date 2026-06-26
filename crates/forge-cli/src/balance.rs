//! Pre-flight provider balance checks. Some API providers expose a credit-balance endpoint
//! reachable with just the API key; when the balance is effectively zero we drop that provider's
//! PAID models from discovery, so the mesh never routes to (or fails over onto) a model the account
//! can't pay for. This stops the "can only afford N tokens" 402 churn at the source — an account
//! with $0 credit simply has no paid models in its catalog.
//!
//! Only providers with a real, key-authenticated balance API are checked (today: OpenRouter and
//! DeepSeek). Providers with no such endpoint — OpenAI, Anthropic, Groq, Gemini, xAI, Cerebras,
//! and the free-curated gateways — return `None` and are left untouched (fail open: never hide a
//! model on an inconclusive probe). Add a provider by giving it an arm in [`remaining_credit`].
//!
//! Keys are resolved via [`forge_config::api_key`], which reads the env var first (populated from
//! the keyring by `inject_provider_keys` at startup) then the OS keyring directly — so a key stored
//! either way is found.

use std::time::Duration;

/// A provider is considered out of credit below this balance (covers float dust / a few cents that
/// can't cover a real request). Compared against the provider's own balance unit; a true zero is
/// zero in any currency.
pub const MIN_CREDIT_USD: f64 = 0.01;

/// Network timeout for a single balance probe — kept short so startup discovery never stalls.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Spendable balance for `provider`, when it exposes a key-authenticated balance API. `None` =
/// unknown (no balance API for this provider, no key configured, or the call failed) → the caller
/// must NOT filter on it (fail open). A returned `0.0` means "confirmed out of credit".
pub async fn remaining_credit(provider: &str) -> Option<f64> {
    match provider {
        "openrouter" => openrouter_credit().await,
        "deepseek" => deepseek_balance().await,
        _ => None,
    }
}

/// OpenRouter `GET /api/v1/credits` → `{ "data": { "total_credits": N, "total_usage": M } }`.
/// Remaining = total_credits − total_usage.
async fn openrouter_credit() -> Option<f64> {
    let body = get_json("openrouter", "https://openrouter.ai/api/v1/credits").await?;
    let total = body["data"]["total_credits"].as_f64()?;
    let used = body["data"]["total_usage"].as_f64().unwrap_or(0.0);
    Some(total - used)
}

/// DeepSeek `GET /user/balance` → `{ "is_available": bool, "balance_infos": [{ "total_balance":
/// "10.00", "currency": "USD", … }] }`. `is_available` is DeepSeek's own "can this account pay for
/// calls" flag — when false we report 0.0 regardless of the numbers; otherwise the summed
/// `total_balance` (a string field, so parsed leniently).
async fn deepseek_balance() -> Option<f64> {
    let body = get_json("deepseek", "https://api.deepseek.com/user/balance").await?;
    if body["is_available"].as_bool() == Some(false) {
        return Some(0.0);
    }
    let infos = body["balance_infos"].as_array()?;
    let total: f64 = infos
        .iter()
        .filter_map(|i| as_number(&i["total_balance"]))
        .sum();
    Some(total)
}

/// GET `url` with the provider's bearer key and parse a JSON body. `None` on any failure (no key,
/// transport error, non-2xx, unparseable) — every balance probe fails open.
async fn get_json(provider: &str, url: &str) -> Option<serde_json::Value> {
    let key = forge_config::api_key(provider)
        .ok()
        .filter(|k| !k.is_empty())?;
    let resp = forge_provider::bundled_http_client()
        .get(url)
        .bearer_auth(&key)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!("{provider} balance endpoint returned {}", resp.status());
        return None;
    }
    resp.json().await.ok()
}

/// A JSON number that may be encoded as a number OR a string (DeepSeek sends `"10.00"`).
fn as_number(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Whether `model_id` is a genuinely-free model for its provider (so it survives a zero-balance
/// filter). Mirrors forge-mesh's `is_free` rule for the gateways we balance-check: on OpenRouter
/// only the `:free`-suffixed variants cost nothing; DeepSeek has no free tier, so every DeepSeek
/// model is metered and dropped when the balance is zero.
pub fn is_free_model_id(model_id: &str) -> bool {
    match forge_config::provider_of(model_id) {
        "openrouter" => model_id.contains(":free"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_free_variants_survive_filter() {
        assert!(is_free_model_id("openrouter::meta-llama/llama-3.1-8b:free"));
        assert!(!is_free_model_id("openrouter::sao10k/l3.1-euryale-70b"));
        // DeepSeek has no free tier — nothing survives a zero-balance filter.
        assert!(!is_free_model_id("deepseek::deepseek-chat"));
        assert!(!is_free_model_id("anthropic::claude-opus-4-8"));
    }

    #[test]
    fn parses_numeric_or_string_balance() {
        assert_eq!(as_number(&serde_json::json!(10.5)), Some(10.5));
        assert_eq!(as_number(&serde_json::json!("10.00")), Some(10.0));
        assert_eq!(as_number(&serde_json::json!("  3.25 ")), Some(3.25));
        assert_eq!(as_number(&serde_json::json!(null)), None);
    }
}
