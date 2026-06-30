//! OAuth 2.0 foundation for OAuth-protected MCP servers (RFC-mcp-oauth, PR1). This module owns the
//! **pure, offline-testable** half: config + token types, PKCE (RFC 7636), the authorize-URL
//! builder (RFC 6749 + 8252 loopback), discovery-metadata parsing (RFC 9728 + 8414), and keyring
//! token storage (ADR-0007 — tokens live in the keyring, never in config/logs).
//!
//! The networked half (metadata fetch, token exchange/refresh, the loopback listener + browser
//! open, connect-time integration) lands in forge-mcp + forge-cli (PR2); it builds on these types.

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ConfigError;

/// Per-server OAuth config (the `[servers.auth.oauth]` table). All optional — discovered at login
/// and persisted back. Presence of this (vs a static token) marks a server as OAuth.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Authorization-server issuer. Discovered from the 401's resource-metadata when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Scopes to request (the server may narrow them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Client id — set after dynamic registration (RFC 7591), or pinned manually.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Pin the loopback redirect port (firewalled hosts); ephemeral (`:0`) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
}

/// Tokens persisted (keyring only) per server under `mcp-oauth:<server>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds when the access token expires (0 = unknown / no expiry).
    #[serde(default)]
    pub expires_at: i64,
    pub token_endpoint: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl OAuthTokens {
    /// Whether the access token is expired (or within `skew` seconds of it) as of `now`. An
    /// `expires_at` of 0 means "unknown" → treated as not-expired (let the server 401 if stale).
    pub fn is_expired(&self, now: i64, skew: i64) -> bool {
        self.expires_at != 0 && now + skew >= self.expires_at
    }
}

/// RFC 9728 Protected Resource Metadata (the 401's `resource_metadata` doc).
#[derive(Debug, Clone, Deserialize)]
pub struct ProtectedResourceMetadata {
    #[serde(default)]
    pub authorization_servers: Vec<String>,
}

/// RFC 8414 Authorization Server Metadata (`/.well-known/oauth-authorization-server`).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

/// A PKCE pair (RFC 7636): the `verifier` is the secret kept locally; the `challenge` is the
/// S256 hash sent in the authorize request and proven later by presenting the verifier.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh PKCE pair: a 32-byte CSPRNG verifier (base64url, ~43 chars) and its
    /// S256 challenge `base64url(sha256(verifier))`.
    pub fn generate() -> Pkce {
        let bytes: [u8; 32] = rand::random();
        let verifier = b64url(&bytes);
        Pkce::from_verifier(verifier)
    }

    /// Build the pair from a given verifier (used by tests against the RFC 7636 vector).
    pub fn from_verifier(verifier: String) -> Pkce {
        let digest = Sha256::digest(verifier.as_bytes());
        Pkce {
            challenge: b64url(&digest),
            verifier,
        }
    }
}

/// A random URL-safe `state` (CSRF guard for the authorize round-trip).
pub fn random_state() -> String {
    let bytes: [u8; 16] = rand::random();
    b64url(&bytes)
}

/// base64url **without padding** (RFC 7636 §A / RFC 4648 §5).
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Percent-encode a query-parameter value (RFC 3986 unreserved stays literal).
fn pct(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the authorization-request URL (RFC 6749 §4.1.1 + PKCE S256). `redirect_uri` is the
/// loopback callback (RFC 8252). Scopes are space-joined; `state` + `challenge` bind the request.
pub fn authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    code_challenge: &str,
) -> String {
    let sep = if authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let scope = scopes.join(" ");
    format!(
        "{authorization_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        pct(client_id),
        pct(redirect_uri),
        pct(&scope),
        pct(state),
        pct(code_challenge),
    )
}

/// Keyring key for a server's OAuth tokens — distinct from the static `mcp:<server>` bearer key.
pub fn oauth_keyring_key(server: &str) -> String {
    format!("mcp-oauth:{server}")
}

/// Persist a server's OAuth tokens (keyring, encrypted-file fallback; ADR-0007: never in
/// config/logs).
pub fn store_oauth_tokens(server: &str, tokens: &OAuthTokens) -> Result<(), ConfigError> {
    let json = serde_json::to_string(tokens).map_err(|e| ConfigError::Keyring(e.to_string()))?;
    crate::secret_store::set(&oauth_keyring_key(server), &json)
}

/// Load a server's OAuth tokens, or `None` if none stored / unreadable.
pub fn load_oauth_tokens(server: &str) -> Option<OAuthTokens> {
    let json = crate::secret_store::get(&oauth_keyring_key(server))?;
    serde_json::from_str(&json).ok()
}

/// Delete a server's stored OAuth tokens (`forge mcp logout`). Idempotent: `Ok(false)` if none.
pub fn clear_oauth_tokens(server: &str) -> Result<bool, ConfigError> {
    crate::secret_store::delete(&oauth_keyring_key(server))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B known-answer test.
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
        assert_eq!(p.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_pkce_is_url_safe_and_verifiable() {
        let p = Pkce::generate();
        assert!(p.verifier.len() >= 43, "verifier ≥43 chars (RFC 7636)");
        assert!(
            !p.verifier.contains(['+', '/', '=']),
            "base64url, no padding"
        );
        assert!(!p.challenge.contains(['+', '/', '=']));
        // The challenge is reproducible from the verifier.
        assert_eq!(
            Pkce::from_verifier(p.verifier.clone()).challenge,
            p.challenge
        );
    }

    #[test]
    fn authorize_url_has_required_params_and_encodes() {
        let url = authorize_url(
            "https://auth.example/authorize",
            "client 1",
            "http://127.0.0.1:8080/callback",
            &["mcp".into(), "offline".into()],
            "xyz",
            "CHAL",
        );
        assert!(url.starts_with("https://auth.example/authorize?response_type=code"));
        assert!(url.contains("client_id=client%201"), "space encoded: {url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fcallback"));
        assert!(url.contains("scope=mcp%20offline"));
        assert!(url.contains("code_challenge=CHAL&code_challenge_method=S256"));
        assert!(url.contains("state=xyz"));
    }

    #[test]
    fn authorize_url_appends_with_amp_when_endpoint_has_query() {
        let url = authorize_url(
            "https://a/x?foo=1",
            "c",
            "http://127.0.0.1/cb",
            &[],
            "s",
            "ch",
        );
        assert!(
            url.contains("x?foo=1&response_type=code"),
            "uses & not ?: {url}"
        );
    }

    #[test]
    fn protected_resource_metadata_parses() {
        let m: ProtectedResourceMetadata = serde_json::from_str(
            r#"{"resource":"https://helm/mcp","authorization_servers":["https://helm"]}"#,
        )
        .unwrap();
        assert_eq!(m.authorization_servers, vec!["https://helm".to_string()]);
    }

    #[test]
    fn auth_server_metadata_parses_with_optional_registration() {
        let m: AuthServerMetadata = serde_json::from_str(
            r#"{"issuer":"https://helm","authorization_endpoint":"https://helm/authorize",
                "token_endpoint":"https://helm/token"}"#,
        )
        .unwrap();
        assert_eq!(m.authorization_endpoint, "https://helm/authorize");
        assert_eq!(m.token_endpoint, "https://helm/token");
        assert!(m.registration_endpoint.is_none());
    }

    #[test]
    fn tokens_round_trip_json_and_expiry_logic() {
        let t = OAuthTokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: 1000,
            token_endpoint: "https://helm/token".into(),
            client_id: "cid".into(),
            scopes: vec!["mcp".into()],
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<OAuthTokens>(&json).unwrap(), t);
        // Expired within the skew window, fresh well before it; 0 = unknown = never expired.
        assert!(t.is_expired(950, 60), "950+60 >= 1000");
        assert!(!t.is_expired(800, 60));
        let unknown = OAuthTokens { expires_at: 0, ..t };
        assert!(!unknown.is_expired(i64::MAX - 1, 60));
    }

    #[test]
    fn keyring_key_is_namespaced() {
        assert_eq!(oauth_keyring_key("helm"), "mcp-oauth:helm");
    }
}
