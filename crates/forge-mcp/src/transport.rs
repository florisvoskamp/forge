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
            let transport =
                TokioChildProcess::new(cmd).map_err(|e| format!("spawn '{command}': {e}"))?;
            ().serve(transport)
                .await
                .map_err(|e| format!("initialize: {e}"))
        }
        McpTransport::Http { url, headers } => {
            let client = build_http_client(headers)?;
            let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.clone());
            if let Some(token) = server.token() {
                cfg = cfg.auth_header(token); // sent as `Authorization: Bearer <token>`
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
