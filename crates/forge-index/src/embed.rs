//! Semantic embeddings for Lattice (code-intelligence.md §5.6): an [`Embedder`] abstraction and an
//! ollama HTTP adapter. The storage + cosine ranking live in `lib.rs`; this is the text→vector
//! half plus its response parsing. Off unless `[lattice.embeddings]` is enabled + a backend runs.

use async_trait::async_trait;

use crate::LatticeError;

/// Turns text into embedding vectors. `OllamaEmbedder` is the real backend; tests use a fake.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed each input, returning one vector per input (same order).
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LatticeError>;
}

/// ollama embeddings adapter — `POST {endpoint}/api/embed` with `{model, input:[...]}`.
pub struct OllamaEmbedder {
    endpoint: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaEmbedder {
    pub fn new(endpoint: &str, model: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: bundled_ca_client(),
        }
    }
}

/// A reqwest client seeded with Mozilla's bundled root CAs. `reqwest::Client::new()` builds against
/// the OS trust store and **panics at construction** on a host that has none (bare container / minimal
/// image) — even for the plain-HTTP ollama endpoint, because the panic is in TLS-backend setup, not at
/// connect time. Building from the bundled certs bypasses that. (forge-index can't depend on
/// forge-provider, which has the same helper.)
fn bundled_ca_client() -> reqwest::Client {
    let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok());
    reqwest::Client::builder()
        .tls_certs_only(certs)
        .build()
        .expect("reqwest client with bundled CA certificates")
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LatticeError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/api/embed", self.endpoint);
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| LatticeError::Io(format!("ollama embed request: {e}")))?;
        if !resp.status().is_success() {
            return Err(LatticeError::Io(format!(
                "ollama embed HTTP {} (is `{}` pulled? `ollama pull {}`)",
                resp.status(),
                self.model,
                self.model
            )));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LatticeError::Io(format!("ollama embed decode: {e}")))?;
        let vecs = parse_ollama_embeddings(&json)
            .ok_or_else(|| LatticeError::Io("ollama embed: unexpected response shape".into()))?;
        if vecs.len() != texts.len() {
            return Err(LatticeError::Io(format!(
                "ollama embed: got {} vectors for {} inputs",
                vecs.len(),
                texts.len()
            )));
        }
        Ok(vecs)
    }
}

/// Parse ollama's `/api/embed` response `{"embeddings": [[f32,...], ...]}` (the newer batch shape),
/// falling back to the legacy single-vector `{"embedding": [..]}`. Pure, so it's unit-tested.
pub fn parse_ollama_embeddings(v: &serde_json::Value) -> Option<Vec<Vec<f32>>> {
    let to_vec = |row: &serde_json::Value| -> Option<Vec<f32>> {
        Some(
            row.as_array()?
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect(),
        )
    };
    if let Some(arr) = v.get("embeddings").and_then(|e| e.as_array()) {
        return arr.iter().map(to_vec).collect();
    }
    // Legacy `/api/embeddings` single-vector form.
    v.get("embedding").map(|e| to_vec(e).map(|v| vec![v]))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_batch_embeddings() {
        let v = serde_json::json!({"embeddings": [[0.1, 0.2], [0.3, 0.4]]});
        let got = parse_ollama_embeddings(&v).unwrap();
        assert_eq!(got, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    }

    #[test]
    fn parses_legacy_single_embedding() {
        let v = serde_json::json!({"embedding": [1.0, 2.0, 3.0]});
        assert_eq!(
            parse_ollama_embeddings(&v).unwrap(),
            vec![vec![1.0, 2.0, 3.0]]
        );
    }

    #[test]
    fn rejects_unexpected_shape() {
        assert!(parse_ollama_embeddings(&serde_json::json!({"oops": 1})).is_none());
    }
}
