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

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3 + PKCE verifier).
/// Returns a full `OAuthTokens` (access + optional refresh, expiry).
pub async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    pkce_verifier: &str,
) -> Result<OAuthTokens, String> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", pkce_verifier),
    ];
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

/// Refresh an access token using a refresh token (RFC 6749 §6).
pub async fn refresh_token(
    client: &reqwest::Client,
    tokens: &OAuthTokens,
) -> Result<OAuthTokens, String> {
    let rt = tokens
        .refresh_token
        .as_deref()
        .ok_or("no refresh token stored — run `forge mcp login <server>`")?;
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", rt),
        ("client_id", tokens.client_id.as_str()),
    ];
    let resp: serde_json::Value = reqwest::Client::new()
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

    let client = reqwest::Client::new();
    let new_tokens = refresh_token(&client, &tokens)
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
