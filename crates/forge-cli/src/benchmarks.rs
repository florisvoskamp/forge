//! Fetch + cache measured model performance from the Artificial Analysis Data API (ADR-0011) and
//! build a [`forge_mesh::BenchmarkScores`] for the mesh to rank on. Network + disk live here (the
//! binary); `forge-mesh` stays pure. Best-effort throughout: any failure yields `None` and the
//! mesh falls back to its family-name heuristic.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_mesh::BenchmarkScores;
use serde_json::{json, Value};

const CACHE_PATH: &str = ".forge/benchmarks.json";
/// Safety backstop only: scores move slowly, so we normally DON'T re-fetch — the trigger is a new
/// catalog model with no rating yet. This long TTL just guarantees the dataset can't go infinitely
/// stale even if the model set never changes.
const SAFETY_TTL_SECS: i64 = 30 * 24 * 3600;
const API_URL: &str = "https://artificialanalysis.ai/api/v2/data/llms/models";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

/// One parsed row: (source model name/slug, intelligence index, coding index), each 0–100.
type Row = (String, f64, f64);

/// Recursively find a numeric field by key anywhere within a model entry (the API nests the
/// indices under an `evaluations`-style object in some versions, flat in others).
fn find_f64(v: &Value, key: &str) -> Option<f64> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key).and_then(Value::as_f64) {
                return Some(x);
            }
            m.values().find_map(|val| find_f64(val, key))
        }
        Value::Array(a) => a.iter().find_map(|e| find_f64(e, key)),
        _ => None,
    }
}

/// Normalise an index to a 0–100 scale (some API versions report 0–1 fractions).
fn norm(x: f64) -> f64 {
    if x <= 1.5 {
        x * 100.0
    } else {
        x
    }
}

/// Parse the API body into rows. Tolerant of `{data:[…]}` vs a bare array and of where the indices
/// live within each entry. A row needs a name and an intelligence index; coding defaults to it.
fn parse_rows(body: &str) -> Vec<Row> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let entries = v
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| v.as_array());
    let Some(entries) = entries else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for e in entries {
        let name = e
            .get("name")
            .or_else(|| e.get("slug"))
            .or_else(|| e.get("model_name"))
            .and_then(Value::as_str);
        let Some(name) = name else { continue };
        let Some(intel) = find_f64(e, "artificial_analysis_intelligence_index") else {
            continue;
        };
        let coding = find_f64(e, "artificial_analysis_coding_index").unwrap_or(intel);
        rows.push((name.to_string(), norm(intel), norm(coding)));
    }
    rows
}

fn rows_to_scores(rows: &[Row]) -> BenchmarkScores {
    let mut b = BenchmarkScores::new();
    for (name, intel, coding) in rows {
        b.insert(name, *intel, *coding);
    }
    b
}

/// The persisted cache: scored rows, a negative cache of catalog ids the API had NO rating for
/// (so an unlisted model — e.g. a local ollama one — doesn't trigger a fetch on every run), and
/// the fetch age in seconds.
struct Cache {
    rows: Vec<Row>,
    unrated: Vec<String>,
    age: i64,
}

fn load_cache() -> Option<Cache> {
    let body = std::fs::read_to_string(CACHE_PATH).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let fetched = v.get("fetched_at").and_then(Value::as_i64)?;
    let rows = v
        .get("models")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|m| {
            let name = m.get("name")?.as_str()?.to_string();
            let intel = m.get("intelligence")?.as_f64()?;
            let coding = m.get("coding").and_then(Value::as_f64).unwrap_or(intel);
            Some((name, intel, coding))
        })
        .collect();
    let unrated = v
        .get("unrated")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(Cache {
        rows,
        unrated,
        age: now() - fetched,
    })
}

fn save_cache(rows: &[Row], unrated: &[String]) {
    let models: Vec<Value> = rows
        .iter()
        .map(|(n, i, c)| json!({ "name": n, "intelligence": i, "coding": c }))
        .collect();
    let doc = json!({ "fetched_at": now(), "models": models, "unrated": unrated });
    let _ = std::fs::create_dir_all(".forge");
    if let Ok(bytes) = serde_json::to_vec_pretty(&doc) {
        let _ = std::fs::write(CACHE_PATH, bytes);
    }
}

async fn fetch_api(key: &str) -> Option<Vec<Row>> {
    let resp = reqwest::Client::new()
        .get(API_URL)
        .header("x-api-key", key)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!("benchmark API returned {}", resp.status());
        return None;
    }
    let body = resp.text().await.ok()?;
    let rows = parse_rows(&body);
    (!rows.is_empty()).then_some(rows)
}

/// Benchmark scores for ranking the given catalog `model_ids`. Incremental + cache-first: cached
/// scores are kept and reused; the API is hit ONLY when a catalog model has no rating yet (a new
/// model) — not on every run — plus a 30-day safety refresh and `force`. Models the API doesn't
/// list are remembered (negative cache) so they don't re-trigger fetches. Disabled when
/// `mesh.benchmark_ranking` is false; `None` when there's neither cache nor a usable fetch.
pub async fn ensure(
    config: &forge_config::Config,
    model_ids: &[String],
    force: bool,
) -> Option<BenchmarkScores> {
    if !config.mesh.benchmark_ranking {
        return None;
    }
    let cached = load_cache();
    let cached_scores = cached.as_ref().map(|c| rows_to_scores(&c.rows));

    // Fetch only when something actually needs it: forced, no cache, a stale-beyond-safety cache,
    // or a catalog model we've neither scored nor already recorded as unrated (i.e. a NEW model).
    let needs_fetch = force
        || match &cached {
            None => true,
            Some(c) => {
                c.age > SAFETY_TTL_SECS
                    || model_ids.iter().any(|m| {
                        !c.unrated.contains(m)
                            && cached_scores
                                .as_ref()
                                .is_none_or(|s| s.score_for(m).is_none())
                    })
            }
        };

    if needs_fetch {
        if let Some(key) = forge_config::benchmark_api_key() {
            if let Some(rows) = fetch_api(&key).await {
                let scores = rows_to_scores(&rows);
                // Record catalog models still unmatched after this fetch, so they don't re-trigger.
                let unrated: Vec<String> = model_ids
                    .iter()
                    .filter(|m| scores.score_for(m).is_none())
                    .cloned()
                    .collect();
                save_cache(&rows, &unrated);
                return Some(scores);
            }
        }
    }
    cached_scores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_and_nested_shapes() {
        let flat = r#"{"data":[{"name":"GPT-5.2","artificial_analysis_intelligence_index":58,"artificial_analysis_coding_index":55}]}"#;
        let r = parse_rows(flat);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], ("GPT-5.2".into(), 58.0, 55.0));

        let nested = r#"[{"slug":"claude-opus","evaluations":{"artificial_analysis_intelligence_index":0.64}}]"#;
        let r = parse_rows(nested);
        assert_eq!(r[0].0, "claude-opus");
        assert_eq!(r[0].1, 64.0, "0–1 fraction normalised to 0–100");
        assert_eq!(r[0].2, 64.0, "coding defaults to intelligence when absent");
    }
}
