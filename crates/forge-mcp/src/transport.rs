//! Build an rmcp client connection for a configured server and complete the MCP `initialize`
//! handshake. Both transports resolve to the same `RunningService<RoleClient, ()>`, so the
//! manager treats stdio and HTTP servers identically once connected.

use forge_config::{McpServerConfig, McpTransport};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;

/// Connect to a server (spawn the stdio child / open the HTTP stream) and run `initialize`.
/// The unit client handler `()` is a passive client: it makes requests, serves none.
pub async fn serve(server: &McpServerConfig) -> Result<RunningService<RoleClient, ()>, String> {
    match &server.transport {
        McpTransport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            // Inject the resolved secret token into the child's environment under its declared
            // var name. The value comes from env/keyring (ADR-0007), never from the TOML.
            if let (Some(token), Some(var)) = (
                server.token(),
                server.auth.as_ref().and_then(|a| a.token_env.clone()),
            ) {
                cmd.env(var, token);
            }
            let transport =
                TokioChildProcess::new(cmd).map_err(|e| format!("spawn '{command}': {e}"))?;
            ().serve(transport)
                .await
                .map_err(|e| format!("initialize: {e}"))
        }
        McpTransport::Http { url, headers } => {
            // Decide where the token rides: a custom auth header (e.g. `X-Goog-Api-Key`) is sent
            // verbatim via the client's default headers; otherwise it's `Authorization: Bearer`.
            // Static token (env/keyring) takes precedence; OAuth token is used when present and
            // no static token is configured (run `forge mcp login <name>` to obtain one).
            let static_token = server.token();
            let custom_header = server
                .auth
                .as_ref()
                .and_then(|a| a.header.clone())
                .filter(|h| !h.eq_ignore_ascii_case("authorization"));
            let mut all_headers = headers.clone();
            let mut bearer = None;
            if let Some(token) = static_token {
                match custom_header {
                    Some(h) => {
                        all_headers.insert(h, token);
                    }
                    None => bearer = Some(token),
                }
            } else if let Some(oauth) = server.auth.as_ref().and_then(|a| a.oauth.as_ref()) {
                // OAuth server: resolve stored tokens (with auto-refresh on expiry).
                let _ = oauth; // config used for presence check; tokens are keyed by server name
                match crate::oauth::resolve_oauth_token_async(&server.name).await {
                    Ok(token) => bearer = Some(token),
                    Err(e) => return Err(e),
                }
            }
            let client = build_http_client(&all_headers)?;
            let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.clone());
            if let Some(b) = bearer {
                cfg = cfg.auth_header(b); // sent as `Authorization: Bearer <token>`
            }
            let transport = StreamableHttpClientTransport::with_client(client, cfg);
            ().serve(transport)
                .await
                .map_err(|e| format!("initialize: {e}"))
        }
    }
}

/// A reqwest client carrying the server's static custom headers as defaults (the bearer token
/// is applied separately via the transport's `auth_header`).
fn build_http_client(
    headers: &std::collections::HashMap<String, String>,
) -> Result<reqwest::Client, String> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name =
            HeaderName::from_bytes(k.as_bytes()).map_err(|e| format!("header '{k}': {e}"))?;
        let val = HeaderValue::from_str(v).map_err(|e| format!("header '{k}' value: {e}"))?;
        map.insert(name, val);
    }
    reqwest::Client::builder()
        .default_headers(map)
        .build()
        .map_err(|e| format!("http client: {e}"))
}
