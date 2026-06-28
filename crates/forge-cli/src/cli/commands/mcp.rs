use anyhow::{Context, Result};
use std::io::IsTerminal;

use crate::*;

/// `forge mcp [tools <server> | import [path]]` — connect to the configured MCP servers and show
/// their status, list one server's tools, or import servers from your installed AI CLIs.
pub(crate) async fn mcp_cmd(cmd: Option<McpCmd>) -> Result<()> {
    // Import / Login / Logout / Agent need no MCP connection. Resolve to the listing path otherwise.
    let tools_server = match cmd {
        Some(McpCmd::Agent { session, cwd }) => {
            return crate::mcp_agent::run(session, cwd).await;
        }
        Some(McpCmd::Import { path }) => return mcp_import(path),
        Some(McpCmd::Login { server }) => return mcp_login(&server).await,
        Some(McpCmd::Logout { server }) => return mcp_logout(&server),
        Some(McpCmd::Tools { server }) => Some(server),
        None => None,
    };

    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    if let Err(e) = config.mcp.validate() {
        anyhow::bail!("{e}");
    }
    if config.mcp.active_servers().next().is_none() {
        println!("no MCP servers configured. Declare them in .forge/mcp.toml, or run `forge mcp import`.");
        return Ok(());
    }

    let manager = forge_mcp::McpManager::connect_all(&config.mcp).await;
    match tools_server {
        Some(server) => {
            let tools = manager.tool_lines(&server);
            if tools.is_empty() {
                println!("no tools for server '{server}' (not connected, or it exposes none)");
            } else {
                println!("{} tool(s) on '{server}':", tools.len());
                for (name, desc) in tools {
                    println!("  {name} — {desc}");
                }
            }
        }
        None => {
            let lines = manager.status_lines();
            println!("MCP servers ({} configured)", lines.len());
            for s in &lines {
                let detail = s
                    .detail
                    .as_deref()
                    .map(|d| format!("  {d}"))
                    .unwrap_or_default();
                println!(
                    "  {:<12} {:<13} {:<6} {} tools · {} resources · {} prompts{detail}",
                    s.name, s.status, s.transport, s.tools, s.resources, s.prompts
                );
            }
            println!(
                "\ntools load on demand — `forge mcp tools <server>` to see a server's full list."
            );
        }
    }
    manager.shutdown().await;
    Ok(())
}

/// `forge mcp import [path]`. With an explicit `path`, import that one JSON file. With no path,
/// auto-scan every installed AI-CLI MCP config (Claude Code/Desktop, Codex, Cursor, Windsurf,
/// VS Code) and let the user pick which servers to import. Selected servers are merged into
/// `.forge/mcp.toml`; secrets are NEVER copied (ADR-0007).
pub(crate) fn mcp_import(path: Option<String>) -> Result<()> {
    let out = std::path::Path::new(".forge/mcp.toml");

    // Explicit single-file import (back-compat / scripting).
    if let Some(src) = path {
        let parsed = forge_config::import_mcp_json(std::path::Path::new(&src))
            .with_context(|| format!("importing {src}"))?;
        return finish_import(out, parsed.servers, parsed.secrets);
    }

    // Auto-scan mode.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let sources = forge_config::discover_import_sources(&cwd);
    if sources.is_empty() {
        println!(
            "No MCP servers found in any known AI-CLI config.\n\
             Scanned: ~/.claude.json, ~/.codex/config.toml, ~/.cursor/mcp.json (+ project), \
             Claude Desktop, Windsurf, ./.mcp.json, ./.vscode/mcp.json.\n\
             You can also import a specific file: `forge mcp import <path-to-.mcp.json>`."
        );
        return Ok(());
    }

    // Flatten + dedup by server name (first source wins), carrying the captured secret from the
    // SAME source the kept server came from.
    let mut flat: Vec<(String, forge_config::McpServerConfig, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for s in &sources {
        for srv in &s.servers {
            if seen.insert(srv.name.clone()) {
                flat.push((
                    s.label.clone(),
                    srv.clone(),
                    s.secrets.get(&srv.name).cloned(),
                ));
            }
        }
    }

    // Pick: animated TUI multi-select on a real terminal; import-all when piped/CI.
    let selection = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let items: Vec<forge_tui::SelectItem> = flat
            .iter()
            .map(|(label, srv, secret)| forge_tui::SelectItem {
                label: srv.name.clone(),
                hint: format!(
                    "[{}]  {}{}",
                    srv.transport_label(),
                    label,
                    if secret.is_some() {
                        "  · token → keyring"
                    } else {
                        ""
                    }
                ),
                preselected: true,
            })
            .collect();
        match forge_tui::select_multi("Import MCP servers", &items)
            .context("running the import picker")?
        {
            None => {
                println!("cancelled — nothing imported.");
                return Ok(());
            }
            Some(idx) => idx,
        }
    } else {
        println!(
            "Discovered {} MCP server(s); importing all (non-interactive).",
            flat.len()
        );
        (0..flat.len()).collect()
    };

    let mut servers = Vec::new();
    let mut secrets = std::collections::HashMap::new();
    for i in selection {
        let (_, srv, secret) = &flat[i];
        if let Some(val) = secret {
            secrets.insert(srv.name.clone(), val.clone());
        }
        servers.push(srv.clone());
    }
    if servers.is_empty() {
        println!("nothing selected.");
        return Ok(());
    }
    finish_import(out, servers, secrets)
}

/// Store each captured token in the OS keyring, merge the servers into `.forge/mcp.toml`, and
/// report. Forge does the secret-handling itself (ADR-0007): the token goes to the keyring, the
/// config only references it — the user is never asked to move anything by hand.
pub(crate) fn finish_import(
    out: &std::path::Path,
    servers: Vec<forge_config::McpServerConfig>,
    secrets: std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut stored = Vec::new();
    let mut store_failed = Vec::new();
    for srv in &servers {
        let Some(value) = secrets.get(&srv.name) else {
            continue;
        };
        let key = srv
            .auth
            .as_ref()
            .and_then(|a| a.token_keyring.clone())
            .unwrap_or_else(|| format!("mcp:{}", srv.name));
        match forge_config::store_secret(&key, value) {
            Ok(()) => stored.push(srv.name.clone()),
            Err(e) => store_failed.push((srv.name.clone(), e.to_string())),
        }
    }

    let mut config = forge_config::load_mcp_toml(out);
    let existing: std::collections::HashSet<String> =
        config.servers.iter().map(|s| s.name.clone()).collect();
    let (mut added, mut skipped) = (Vec::new(), Vec::new());
    for srv in servers {
        if existing.contains(&srv.name) {
            skipped.push(srv.name);
        } else {
            added.push(srv.name.clone());
            config.servers.push(srv);
        }
    }
    forge_config::write_mcp_toml(out, &config).context("writing .forge/mcp.toml")?;

    if added.is_empty() {
        println!(
            "nothing new imported (all selected servers already in {}).",
            out.display()
        );
    } else {
        println!(
            "✓ imported {} server(s) → {}: {}",
            added.len(),
            out.display(),
            added.join(", ")
        );
    }
    if !skipped.is_empty() {
        println!("  • skipped (already present): {}", skipped.join(", "));
    }
    if !stored.is_empty() {
        println!(
            "  🔐 stored {} token(s) in the OS keyring: {}",
            stored.len(),
            stored.join(", ")
        );
    }
    for (name, err) in &store_failed {
        println!(
            "  ⚠ couldn't store '{name}' token in the keyring ({err}) — export it via the server's \
             token_env, or run `forge auth`. The server is imported but won't authenticate yet."
        );
    }
    Ok(())
}

/// Remove a server's stored OAuth tokens (`forge mcp logout <server>`).
pub(crate) fn mcp_logout(server: &str) -> Result<()> {
    match forge_config::clear_oauth_tokens(server) {
        Ok(true) => println!("✓ OAuth tokens for '{server}' removed from the keyring."),
        Ok(false) => println!("no stored OAuth tokens found for '{server}'."),
        Err(e) => anyhow::bail!("keyring error: {e}"),
    }
    Ok(())
}

/// Interactive OAuth 2.0 login for an OAuth-protected MCP server (`forge mcp login <server>`).
/// Opens the authorization URL in the user's browser, starts a loopback listener for the
/// redirect, exchanges the code for tokens, and stores them in the OS keyring (ADR-0007).
pub(crate) async fn mcp_login(server: &str) -> Result<()> {
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();

    // Find the server by name.
    let srv = config
        .mcp
        .servers
        .iter()
        .find(|s| s.name == server)
        .ok_or_else(|| anyhow::anyhow!("no server '{server}' in .forge/mcp.toml"))?;

    // Must have an oauth config entry.
    let oauth_cfg = srv
        .auth
        .as_ref()
        .and_then(|a| a.oauth.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "server '{server}' has no [auth.oauth] config — add it to .forge/mcp.toml"
            )
        })?;

    let http = forge_provider::bundled_http_client();

    // Discover the authorization server issuer.
    let issuer = if let Some(i) = &oauth_cfg.issuer {
        i.clone()
    } else {
        // Probe the server's well-known resource-metadata endpoint (RFC 9728).
        let url = match &srv.transport {
            forge_config::McpTransport::Http { url, .. } => {
                let base = url.trim_end_matches('/');
                format!("{base}/.well-known/oauth-protected-resource/mcp")
            }
            _ => anyhow::bail!("OAuth login only supported for HTTP transports"),
        };
        println!("Discovering auth server from {url} …");
        let meta = forge_mcp::oauth::fetch_resource_metadata(&http, &url)
            .await
            .map_err(|e| anyhow::anyhow!("fetching resource metadata from {url}: {e}"))?;
        meta.authorization_servers
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("resource metadata has no authorization_servers"))?
    };

    println!("Auth server: {issuer}");

    // Fetch auth server metadata (RFC 8414).
    let as_meta = forge_mcp::oauth::fetch_auth_server_metadata(&http, &issuer)
        .await
        .map_err(|e| anyhow::anyhow!("fetching auth server metadata from {issuer}: {e}"))?;

    // Choose client_id (from config or a fallback public client).
    let client_id = oauth_cfg
        .client_id
        .clone()
        .unwrap_or_else(|| "forge-mcp-client".to_string());

    // Bind a loopback listener to get the redirect port.
    let redirect_port = oauth_cfg.redirect_port.unwrap_or(0);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", redirect_port))
        .await
        .context("binding loopback redirect listener")?;
    let bound_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{bound_port}/callback");

    // PKCE + state.
    let pkce = forge_config::Pkce::generate();
    let state = forge_config::random_state();
    let scopes = if oauth_cfg.scopes.is_empty() {
        vec!["mcp".to_string(), "offline_access".to_string()]
    } else {
        oauth_cfg.scopes.clone()
    };

    let auth_url = forge_config::authorize_url(
        &as_meta.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &scopes,
        &state,
        &pkce.challenge,
    );

    // Open the browser (cross-platform).
    println!("Opening browser for authorization …\n  {auth_url}");
    if let Err(e) = open_browser(&auth_url) {
        println!("(could not open browser automatically: {e})");
        println!("Please open the URL above manually.");
    }

    // Wait for the redirect callback on the loopback listener.
    println!("Waiting for authorization callback on http://127.0.0.1:{bound_port}/callback …");
    let (mut stream, _) =
        tokio::time::timeout(std::time::Duration::from_secs(120), listener.accept())
            .await
            .context("timed out waiting for OAuth callback (120 s)")?
            .context("accepting callback connection")?;

    // Read the HTTP request line to extract `code` and `state`.
    let (code, returned_state) = read_callback_params(&mut stream).await?;

    // Send a minimal HTTP 200 response so the browser doesn't show an error.
    let _ = tokio::io::AsyncWriteExt::write_all(
        &mut stream,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
          <html><body><h2>Authorization complete. You can close this tab.</h2></body></html>",
    )
    .await;
    drop(stream);

    // CSRF check.
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF. Login aborted.");
    }

    // Exchange the code for tokens.
    println!("Exchanging authorization code …");
    let tokens = forge_mcp::oauth::exchange_code(
        &http,
        &as_meta.token_endpoint,
        &code,
        &redirect_uri,
        &client_id,
        &pkce.verifier,
    )
    .await
    .map_err(|e| anyhow::anyhow!("token exchange: {e}"))?;

    // Store in keyring.
    forge_config::store_oauth_tokens(server, &tokens).context("storing OAuth tokens in keyring")?;

    println!("✓ OAuth tokens stored for '{server}'. Forge will refresh them automatically.");
    Ok(())
}

/// Parse `code` and `state` query params from the loopback HTTP GET request.
pub(crate) async fn read_callback_params(
    stream: &mut tokio::net::TcpStream,
) -> Result<(String, String)> {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .context("reading callback request")?;
    let request = std::str::from_utf8(&buf[..n]).unwrap_or_default();
    // First line: `GET /callback?code=XYZ&state=ABC HTTP/1.1`
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default();
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(url_decode(v)),
            "state" => state = Some(url_decode(v)),
            _ => {}
        }
    }
    let code = code.ok_or_else(|| anyhow::anyhow!("no `code` in OAuth callback URL"))?;
    let state = state.ok_or_else(|| anyhow::anyhow!("no `state` in OAuth callback URL"))?;
    Ok((code, state))
}

/// Minimal percent-decode (ASCII only, handles `%XX` and `+` → space).
pub(crate) fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Open `url` in the default system browser (cross-platform best-effort).
pub(crate) fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Linux / BSD: try xdg-open, then sensible-browser, then wslview.
        let browsers = ["xdg-open", "sensible-browser", "wslview"];
        let mut launched = false;
        for b in browsers {
            if std::process::Command::new(b).arg(url).spawn().is_ok() {
                launched = true;
                break;
            }
        }
        if !launched {
            return Err(
                "no browser launcher found (tried xdg-open, sensible-browser, wslview)".into(),
            );
        }
    }
    Ok(())
}
