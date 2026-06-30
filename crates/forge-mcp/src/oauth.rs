//! OAuth 2.0 network operations for MCP OAuth (RFC-mcp-oauth PR2). Handles the networked half:
//! resource-metadata + auth-server-metadata discovery, token exchange, and refresh. The pure types
//! (PKCE, authorize URL, token storage) live in `forge_config::oauth`.

use forge_config::{AuthServerMetadata, OAuthTokens, ProtectedResourceMetadata};

/// Fetch RFC 9728 protected-resource metadata from `url` (the value of `resource_metadata` in
/// the 401 `WWW-Authenticate` header, or a constructed well-known URL).
pub async fn fetch_resource_metadata(
    client: &reqwest::Client,
    url: &str,
) -> Result<ProtectedResourceMetadata, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .json::<ProtectedResourceMetadata>()
        .await
        .map_err(|e| format!("parse resource metadata from {url}: {e}"))
}

/// Fetch RFC 8414 authorization-server metadata from `<issuer>/.well-known/oauth-authorization-server`.
pub async fn fetch_auth_server_metadata(
    client: &reqwest::Client,
    issuer: &str,
) -> Result<AuthServerMetadata, String> {
    let issuer = issuer.trim_end_matches('/');
    let url = format!("{issuer}/.well-known/oauth-authorization-server");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        // Some providers put it under /.well-known/openid-configuration — try that.
        let url2 = format!("{issuer}/.well-known/openid-configuration");
        return client
            .get(&url2)
            .send()
            .await
            .map_err(|e| format!("GET {url2}: {e}"))?
            .json::<AuthServerMetadata>()
            .await
            .map_err(|e| format!("parse auth server metadata from {url2}: {e}"));
    }
    resp.json::<AuthServerMetadata>()
        .await
        .map_err(|e| format!("parse auth server metadata from {url}: {e}"))
}

/// A client registered with an authorization server (RFC 7591). `client_secret` is `None` for a
/// public/PKCE client (`token_endpoint_auth_method=none`), `Some` for a confidential client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_secret: Option<String>,
}

impl RegisteredClient {
    fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let client_id = v
            .get("client_id")
            .and_then(|x| x.as_str())
            .ok_or("registration response missing client_id")?
            .to_string();
        let client_secret = v
            .get("client_secret")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        Ok(Self {
            client_id,
            client_secret,
        })
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "client_id": self.client_id,
            "client_secret": self.client_secret,
        })
    }
}

/// RFC 7591 §3.1 Dynamic Client Registration: POST client metadata to the authorization server's
/// `registration_endpoint` (discovered via RFC 8414 metadata) and get back a real `client_id`
/// (+ optional `client_secret`). Hosted OAuth MCP servers (GitHub, Linear, Notion) reject the old
/// hardcoded public client id, so first-time auth must register a real client here. Registers as a
/// public PKCE client (`token_endpoint_auth_method=none`); a server that insists on issuing a
/// secret has it captured and persisted too.
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uris: &[String],
    scopes: &[String],
    client_name: &str,
) -> Result<RegisteredClient, String> {
    let body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": scopes.join(" "),
    });
    let resp = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST {registration_endpoint}: {e}"))?;
    let status = resp.status();
    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("parse registration response: {e}"))?;
    if !status.is_success() {
        let err = val
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("registration failed");
        let desc = val
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(format!(
            "dynamic client registration failed ({status}): {err} {desc}"
        ));
    }
    RegisteredClient::from_json(&val)
}

/// Keyring key for a server's registered OAuth client (distinct from the tokens key).
pub fn registered_client_key(server: &str) -> String {
    format!("mcp-oauth-client:{server}")
}

/// Persist a registered client (id + optional secret) so subsequent connects/logins reuse it
/// instead of re-registering. Stored in the same secret store as tokens (keyring, encrypted-file
/// fallback) — never in config or logs.
pub fn store_registered_client(server: &str, c: &RegisteredClient) -> Result<(), String> {
    let json = c.to_json().to_string();
    forge_config::store_secret(&registered_client_key(server), &json)
        .map_err(|e| format!("storing registered client: {e}"))
}

/// Load a server's previously-registered client, or `None` if it has never registered.
pub fn load_registered_client(server: &str) -> Option<RegisteredClient> {
    let json = forge_config::load_secret(&registered_client_key(server))?;
    let val: serde_json::Value = serde_json::from_str(&json).ok()?;
    RegisteredClient::from_json(&val).ok()
}

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3 + PKCE verifier). `client_secret` is
/// included only for a confidential client (a DCR-issued secret); public PKCE clients pass `None`.
/// Returns a full `OAuthTokens` (access + optional refresh, expiry).
pub async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    pkce_verifier: &str,
    client_secret: Option<&str>,
) -> Result<OAuthTokens, String> {
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", pkce_verifier),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let resp: serde_json::Value = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("POST {token_endpoint}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse token response: {e}"))?;

    parse_token_response(resp, token_endpoint, client_id)
}

/// Refresh an access token using a refresh token (RFC 6749 §6). `client_secret` (a DCR-issued
/// secret) is included when present for confidential clients.
pub async fn refresh_token(
    client: &reqwest::Client,
    tokens: &OAuthTokens,
    client_secret: Option<&str>,
) -> Result<OAuthTokens, String> {
    let rt = tokens
        .refresh_token
        .as_deref()
        .ok_or("no refresh token stored — run `forge mcp login <server>`")?;
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", rt),
        ("client_id", tokens.client_id.as_str()),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let resp: serde_json::Value = client
        .post(&tokens.token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("POST {}: {e}", tokens.token_endpoint))?
        .json()
        .await
        .map_err(|e| format!("parse refresh response: {e}"))?;

    // A refresh response may or may not include a new refresh token; keep the old one if absent.
    let mut new_tokens = parse_token_response(resp, &tokens.token_endpoint, &tokens.client_id)?;
    if new_tokens.refresh_token.is_none() {
        new_tokens.refresh_token = tokens.refresh_token.clone();
    }
    Ok(new_tokens)
}

/// Load stored OAuth tokens for a server, refresh if expired, and return the valid access token.
/// Returns `Err` with a user-facing message when no tokens or refresh fails.
pub fn resolve_oauth_token(server_name: &str) -> Result<String, String> {
    let tokens = forge_config::load_oauth_tokens(server_name).ok_or_else(|| {
        format!("no OAuth tokens for '{server_name}' — run `forge mcp login {server_name}`")
    })?;

    // Check expiry with a 60 s clock skew.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if !tokens.is_expired(now, 60) {
        return Ok(tokens.access_token.clone());
    }

    // Need refresh — can't do it here (sync context); caller must use resolve_oauth_token_async.
    Err(format!(
        "OAuth token for '{server_name}' is expired — \
         call resolve_oauth_token_async to refresh, or run `forge mcp login {server_name}`"
    ))
}

/// Async version: load stored tokens, refresh if expired, store updated tokens, return access token.
pub async fn resolve_oauth_token_async(server_name: &str) -> Result<String, String> {
    let tokens = forge_config::load_oauth_tokens(server_name).ok_or_else(|| {
        format!("no OAuth tokens for '{server_name}' — run `forge mcp login {server_name}`")
    })?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if !tokens.is_expired(now, 60) {
        return Ok(tokens.access_token.clone());
    }

    let client = crate::transport::bundled_client_builder()
        .build()
        .map_err(|e| format!("http client for '{server_name}': {e}"))?;
    // Pass the DCR-issued client secret (if this server registered a confidential client) so the
    // refresh authenticates; public PKCE clients have no secret and pass `None`.
    let client_secret = load_registered_client(server_name).and_then(|c| c.client_secret);
    let new_tokens = refresh_token(&client, &tokens, client_secret.as_deref())
        .await
        .map_err(|e| format!("token refresh for '{server_name}' failed: {e}"))?;
    forge_config::store_oauth_tokens(server_name, &new_tokens)
        .map_err(|e| format!("storing refreshed tokens: {e}"))?;
    Ok(new_tokens.access_token)
}

/// Parse `access_token`, `refresh_token`, `expires_in`, from a token-endpoint JSON response.
fn parse_token_response(
    resp: serde_json::Value,
    token_endpoint: &str,
    client_id: &str,
) -> Result<OAuthTokens, String> {
    if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
        let desc = resp
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(format!("token error {err}: {desc}"));
    }
    let access_token = resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token in token response")?
        .to_string();
    let refresh_token = resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let expires_at = if let Some(secs) = resp.get("expires_in").and_then(|v| v.as_i64()) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now + secs
    } else {
        0
    };
    let scope_str = resp
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let scopes = scope_str
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        token_endpoint: token_endpoint.to_string(),
        client_id: client_id.to_string(),
        scopes,
    })
}
