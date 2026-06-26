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
            let mut cmd = stdio_command(command, args);
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

/// Build the command to launch a stdio MCP server `command` with `args`. On Windows the server
/// command is often an npm-installed `.cmd` shim (`npx`, and most node-based CLIs), which
/// `CreateProcess` cannot launch directly — resolve it on `PATH` and, when it's a `.cmd`/`.bat`, run
/// it through `cmd /S /C` with the whole command line individually quoted. `cmd` strips the first/last
/// quote of its `/C` string, so a quoted shim path breaks the moment a second quoted token (an arg
/// with a space) appears — `/S` + per-token quoting keeps spaces in the path AND args intact. On Unix
/// this is a plain `Command::new(command).args(args)`.
fn stdio_command(command: &str, args: &[String]) -> tokio::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(p) = resolve_on_path(command) {
            let is_script = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
            if is_script {
                let mut cmd = tokio::process::Command::new("cmd");
                cmd.as_std_mut().raw_arg("/S");
                cmd.as_std_mut().raw_arg("/C");
                cmd.as_std_mut().raw_arg(windows_cmd_line(&p, args));
                return cmd;
            }
        }
    }
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    cmd
}

/// The raw command line for `cmd /S /C` launching `program` (a resolved `.cmd`/`.bat`) with `args`:
/// every token double-quoted (embedded quotes doubled, per `cmd`), wrapped in an outer pair `/S`
/// strips. Pure + cross-platform so it can be unit-tested off Windows.
#[cfg(any(windows, test))]
fn windows_cmd_line(program: &std::path::Path, args: &[String]) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut inner = q(&program.to_string_lossy());
    for a in args {
        inner.push(' ');
        inner.push_str(&q(a));
    }
    format!("\"{inner}\"")
}

/// Resolve `bin` to an executable file on `PATH`, Windows-aware (also tries `.exe`/`.cmd`/`.bat`,
/// since npm installs CLIs as `.cmd` shims a bare-name lookup misses). A `bin` containing a path
/// separator is checked directly. Returns the matched path, or `None`.
#[cfg_attr(not(windows), allow(dead_code))]
fn resolve_on_path(bin: &str) -> Option<std::path::PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let try_base = |base: &std::path::Path| -> Option<std::path::PathBuf> {
        exts.iter().find_map(|e| {
            let cand = if e.is_empty() {
                base.to_path_buf()
            } else {
                let mut s = base.as_os_str().to_owned();
                s.push(e);
                std::path::PathBuf::from(s)
            };
            cand.is_file().then_some(cand)
        })
    };
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 {
        return try_base(p);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| try_base(&dir.join(bin)))
}

/// A reqwest `ClientBuilder` pre-seeded with Mozilla's bundled root CAs, so MCP HTTPS (the
/// streamable-http transport AND the OAuth flow) works on bare/CA-less hosts. Mirrors
/// forge-provider's `build_reqwest_client`; forge-mcp can't depend on forge-provider, and a plain
/// `reqwest::Client::new()`/`builder()` trusts the OS store and **panics** where there is none.
pub(crate) fn bundled_client_builder() -> reqwest::ClientBuilder {
    let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok());
    reqwest::Client::builder().tls_certs_only(certs)
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
    bundled_client_builder()
        .default_headers(map)
        .build()
        .map_err(|e| format!("http client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_client_builder_builds_without_the_os_trust_store() {
        // The whole point: building must succeed from the bundled CAs alone, so MCP HTTPS works on a
        // bare/CA-less host where reqwest's default OS-trust-store path panics. Also assert it loaded
        // a non-trivial cert set.
        assert!(
            !webpki_root_certs::TLS_SERVER_ROOT_CERTS.is_empty(),
            "bundled root CAs present"
        );
        assert!(
            bundled_client_builder().build().is_ok(),
            "client builds from bundled CAs"
        );
    }

    #[test]
    fn build_http_client_uses_the_bundled_ca_path() {
        // build_http_client routes through bundled_client_builder, so it too builds on a CA-less host.
        let headers = std::collections::HashMap::from([("X-Test".to_string(), "1".to_string())]);
        assert!(build_http_client(&headers).is_ok());
    }

    #[test]
    fn resolve_on_path_handles_explicit_files() {
        let dir = std::env::temp_dir().join(format!("forge-mcp-resolve-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("marker.txt");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(
            resolve_on_path(f.to_str().unwrap()).as_deref(),
            Some(f.as_path())
        );
        assert!(resolve_on_path(dir.join("nope").to_str().unwrap()).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn stdio_command_wraps_cmd_shims() {
        // npm-style `.cmd` server commands (e.g. `npx`) must launch via `cmd /C`, not directly.
        let dir = std::env::temp_dir().join(format!("forge-mcp-cmd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let shim = dir.join("npx.cmd");
        std::fs::write(&shim, b"@echo off\n").unwrap();
        let cmd = stdio_command(shim.to_str().unwrap(), &["-y".into()]);
        assert_eq!(cmd.as_std().get_program(), std::ffi::OsStr::new("cmd"));
    }

    #[test]
    fn windows_cmd_line_quotes_path_and_every_arg() {
        let p = std::path::Path::new(r"C:\Users\First Last\npm\npx.cmd");
        let line = windows_cmd_line(p, &["-y".into(), "some pkg".into()]);
        assert_eq!(
            line,
            r#"""C:\Users\First Last\npm\npx.cmd" "-y" "some pkg"""#
        );
        assert!(line.starts_with("\"\"") && line.ends_with("\"\""));
    }
}
