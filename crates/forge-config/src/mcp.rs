//! MCP-client configuration (docs/features/mcp-client.md): declaring external MCP servers,
//! the allowlist, token resolution (env/keyring — never inline in TOML, ADR-0007), and a
//! Claude-Code-compatible `.mcp.json` importer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

fn default_call_timeout_secs() -> u64 {
    60
}
fn default_connect_timeout_secs() -> u64 {
    20
}
fn default_max_reconnect_attempts() -> usize {
    3
}
fn default_true() -> bool {
    true
}

/// The `[mcp]` config section: declared servers + global knobs. Empty (no servers) means the
/// whole MCP path is inert — zero overhead for users who don't use MCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// Allowlist: if `servers` is non-empty, only those server names may connect; if `tools` is
    /// non-empty, only those qualified tool names may be exposed/called.
    #[serde(default)]
    pub allow: McpAllowlist,
    /// Per-`tools/call` timeout (default 60s) — a slow/hung server returns a tool error, not a hang.
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
    /// Connect/initialize budget per server (default 20s) — a slow server lands `failed`/`connecting`
    /// without delaying session start beyond this.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Bounded reconnect attempts after a stdio child exits / an HTTP stream drops (default 3).
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: usize,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            allow: McpAllowlist::default(),
            call_timeout_secs: default_call_timeout_secs(),
            connect_timeout_secs: default_connect_timeout_secs(),
            max_reconnect_attempts: default_max_reconnect_attempts(),
        }
    }
}

impl McpConfig {
    /// Servers that are enabled AND pass the server allowlist. The set Forge actually connects to.
    pub fn active_servers(&self) -> impl Iterator<Item = &McpServerConfig> {
        self.servers
            .iter()
            .filter(|s| s.enabled && self.server_allowed(&s.name))
    }

    /// Is this server permitted by the allowlist? Empty `allow.servers` = all declared servers.
    pub fn server_allowed(&self, name: &str) -> bool {
        self.allow.servers.is_empty() || self.allow.servers.iter().any(|s| s == name)
    }

    /// Is this qualified tool (`server__tool`) permitted? Empty `allow.tools` = every tool of an
    /// allowed server. Otherwise the qualified name must be listed explicitly.
    pub fn tool_allowed(&self, qualified: &str) -> bool {
        self.allow.tools.is_empty() || self.allow.tools.iter().any(|t| t == qualified)
    }

    /// Reject duplicate server names (they'd collide as tool-name prefixes) and empty names.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.servers {
            if s.name.trim().is_empty() {
                return Err("mcp: a server has an empty name".to_string());
            }
            if !seen.insert(&s.name) {
                return Err(format!("mcp: duplicate server name '{}'", s.name));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique namespace prefix for this server's tools (`<name>__<tool>`).
    pub name: String,
    pub transport: McpTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuth>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl McpServerConfig {
    /// Resolve this server's bearer token from env/keyring (ADR-0007). `None` if no auth declared.
    pub fn token(&self) -> Option<String> {
        self.auth.as_ref().and_then(resolve_token)
    }

    /// "stdio" / "http", for status display.
    pub fn transport_label(&self) -> &'static str {
        match self.transport {
            McpTransport::Stdio { .. } => "stdio",
            McpTransport::Http { .. } => "http",
            McpTransport::Sse { .. } => "sse",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    /// A child process speaking MCP over stdio.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// A remote MCP server over streamable-HTTP.
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// A remote MCP server over the legacy HTTP+SSE transport (`GET` event-stream + `POST`
    /// endpoint). Connects via Forge's hand-rolled SSE client (forge-mcp `sse.rs`), since rmcp
    /// ships no standalone SSE client transport.
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Where a server's token comes from — never the value itself in config (ADR-0007).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpAuth {
    /// Environment variable holding the token (e.g. `GITLAB_TOKEN`). For an **stdio** server the
    /// resolved token is injected into the child's environment under this name; for an **http**
    /// server it is sent as a request header (see `header`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Keyring entry name (looked up under the `forge` service), e.g. `mcp:gitlab`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_keyring: Option<String>,
    /// HTTP only: the request header the token rides in. `None` / `Authorization` → sent as
    /// `Authorization: Bearer <token>`; any other name → sent verbatim as `<header>: <token>`
    /// (for servers that use a custom key header, e.g. `X-Goog-Api-Key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// OAuth 2.0 (RFC-mcp-oauth): when set, the server authenticates via the OAuth flow rather
    /// than a static token — `forge mcp login <server>` obtains + refreshes a bearer (PR2).
    /// Resolution prefers a static token (env/keyring) first, then OAuth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<crate::oauth::OAuthConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpAllowlist {
    #[serde(default)]
    pub servers: Vec<String>,
    /// Qualified `server__tool` names.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Resolve a token: env var first, then keyring. `None` if neither yields a non-empty value.
pub fn resolve_token(auth: &McpAuth) -> Option<String> {
    if let Some(var) = &auth.token_env {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    if let Some(key) = &auth.token_keyring {
        if let Some(v) = crate::secret_store::get(key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Env-var name patterns that almost certainly hold a secret — used by the importer to avoid
/// copying a plaintext token out of `.mcp.json` into Forge's TOML.
fn looks_secret(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PAT",
        "CREDENTIAL",
    ]
    .iter()
    .any(|m| k.contains(m))
}

/// The result of parsing one tool's MCP config: the servers (with secrets *referenced*, never
/// embedded), human notes, and the captured secret **values** keyed by server name. The values
/// stay in memory only — the importer writes them to the OS keyring (ADR-0007), never to TOML.
#[derive(Debug, Clone, Default)]
pub struct ParsedServers {
    pub servers: Vec<McpServerConfig>,
    pub notes: Vec<String>,
    /// server name → token value Forge will store in the keyring under that server's `mcp:<name>`.
    pub secrets: HashMap<String, String>,
}

/// Strip a leading `Bearer ` (case-insensitive) so a stored Authorization token is the bare
/// credential — Forge re-adds the `Bearer ` scheme when it sends the request.
fn strip_bearer(v: &str) -> String {
    let t = v.trim();
    t.strip_prefix("Bearer ")
        .or_else(|| t.strip_prefix("bearer "))
        .unwrap_or(t)
        .to_string()
}

/// Parse one server entry from a JSON spec (the `{type?, command/args/env | url/headers}` object
/// used by Claude Code, Cursor, Windsurf, …). A secret is **referenced** in the returned config
/// (`token_keyring = "mcp:<name>"`) and its value captured into `out.secrets[name]`; it is never
/// embedded in the config. Returns `None` for an entry that is neither stdio nor http.
fn server_from_json(name: &str, spec: &serde_json::Value, out: &mut ParsedServers) {
    let keyring_key = format!("mcp:{name}");
    let (transport, auth) = if let Some(cmd) = spec.get("command").and_then(|v| v.as_str()) {
        let args = spec
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut env = HashMap::new();
        let mut auth: Option<McpAuth> = None;
        if let Some(env_obj) = spec.get("env").and_then(|v| v.as_object()) {
            for (k, v) in env_obj {
                let val = v.as_str().unwrap_or("");
                if looks_secret(k) && !val.is_empty() {
                    // One keyring slot per server (`mcp:<name>`), so only the FIRST secret env can be
                    // stored. A second one used to SILENTLY overwrite the first (last-wins) — now it's
                    // kept deterministically (first-wins) and any extra is flagged loudly, NOT dropped
                    // into plain env (which would expose it). Multiple secrets need separate servers.
                    if auth.is_some() {
                        out.notes.push(format!(
                            "server '{name}': IGNORING extra secret env '{k}' — a stdio server has \
                             one keyring slot ('{keyring_key}'), already used. Split into separate \
                             servers (one secret each) so '{k}' isn't lost."
                        ));
                        continue;
                    }
                    // Capture the value for the keyring; reference it by env-var name. At connect,
                    // the resolved token is injected into the child's env under `k`.
                    out.secrets.insert(name.to_string(), val.to_string());
                    auth = Some(McpAuth {
                        token_env: Some(k.clone()),
                        token_keyring: Some(keyring_key.clone()),
                        header: None,
                        oauth: None,
                    });
                    out.notes.push(format!(
                        "server '{name}': storing secret env '{k}' in the keyring"
                    ));
                } else {
                    env.insert(k.clone(), val.to_string());
                }
            }
        }
        (
            McpTransport::Stdio {
                command: cmd.to_string(),
                args,
                env,
            },
            auth,
        )
    } else if let Some(url) = spec.get("url").and_then(|v| v.as_str()) {
        let mut headers = HashMap::new();
        let mut auth: Option<McpAuth> = None;
        if let Some(h) = spec.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in h {
                let val = v.as_str().unwrap_or("");
                let is_auth = k.eq_ignore_ascii_case("authorization");
                if (looks_secret(k) || is_auth) && !val.is_empty() {
                    // Capture the token; send via Authorization-Bearer or the original custom header.
                    let token = if is_auth {
                        strip_bearer(val)
                    } else {
                        val.to_string()
                    };
                    out.secrets.insert(name.to_string(), token);
                    auth = Some(McpAuth {
                        token_env: None,
                        token_keyring: Some(keyring_key.clone()),
                        header: (!is_auth).then(|| k.clone()),
                        oauth: None,
                    });
                    out.notes.push(format!(
                        "server '{name}': storing header '{k}' token in the keyring"
                    ));
                } else {
                    headers.insert(k.clone(), val.to_string());
                }
            }
        }
        (
            McpTransport::Http {
                url: url.to_string(),
                headers,
            },
            auth,
        )
    } else {
        out.notes.push(format!(
            "server '{name}': skipped — neither `command` (stdio) nor `url` (http)"
        ));
        return;
    };
    out.servers.push(McpServerConfig {
        name: name.to_string(),
        transport,
        auth,
        enabled: true,
    });
}

/// Parse a JSON `mcpServers` (Claude/Cursor/Windsurf) **or** `servers` (VS Code) object.
fn servers_from_json(root: &serde_json::Value) -> ParsedServers {
    let mut out = ParsedServers::default();
    if let Some(obj) = root
        .get("mcpServers")
        .or_else(|| root.get("servers"))
        .and_then(|v| v.as_object())
    {
        for (name, spec) in obj {
            server_from_json(name, spec, &mut out);
        }
    }
    out
}

/// Translate a Claude-Code-style `.mcp.json` into an [`McpConfig`] plus notes + captured secret
/// values (to store in the keyring). Secrets are never written into the config (ADR-0007).
pub fn import_mcp_json(path: &Path) -> Result<ParsedServers, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Write(format!("reading {}: {e}", path.display())))?;
    let root: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ConfigError::Write(format!("parsing {}: {e}", path.display())))?;
    if root
        .get("mcpServers")
        .or_else(|| root.get("servers"))
        .is_none()
    {
        return Err(ConfigError::Write(
            "no `mcpServers` (or `servers`) object in the file".into(),
        ));
    }
    Ok(servers_from_json(&root))
}

/// One place Forge found MCP servers declared (a specific tool's config file). Surfaced by
/// [`discover_import_sources`] so the user can pick which servers to import.
#[derive(Debug, Clone)]
pub struct ImportSource {
    /// Human label, e.g. `claude-code (global)`, `codex`, `cursor (project)`.
    pub label: String,
    pub path: PathBuf,
    pub servers: Vec<McpServerConfig>,
    /// Notes (e.g. which secrets will be stored in the keyring).
    pub notes: Vec<String>,
    /// server name → captured token value (in-memory only; the importer writes these to the OS
    /// keyring under `mcp:<name>`, never to TOML).
    pub secrets: HashMap<String, String>,
}

/// Scan every AI-CLI MCP config Forge knows about (Claude Code, Claude Desktop, Codex, Cursor,
/// Windsurf, VS Code) and return the sources that exist and declare ≥1 server. Read-only.
/// Secrets are stripped during parsing — an [`ImportSource`]'s servers never carry a token value.
pub fn discover_import_sources(cwd: &Path) -> Vec<ImportSource> {
    let base = directories::BaseDirs::new();
    let home = base.as_ref().map(|b| b.home_dir());
    let config_dir = base.as_ref().map(|b| b.config_dir());
    discover_in(cwd, home, config_dir)
}

/// Discovery core with the home + config directories injected, so tests can point it at a fake
/// tree without env-var games (`directories` resolves Windows home via the known-folder API, which
/// ignores `HOME`, so an env override only works on Unix).
fn discover_in(cwd: &Path, home: Option<&Path>, config_dir: Option<&Path>) -> Vec<ImportSource> {
    let mut out = Vec::new();

    // --- Claude Code: ~/.claude.json (global `mcpServers` + per-project) ---
    if let Some(home) = home {
        let claude = home.join(".claude.json");
        if let Ok(text) = std::fs::read_to_string(&claude) {
            if let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) {
                push_source(
                    &mut out,
                    "claude-code (global)",
                    &claude,
                    servers_from_json(&root),
                );
                // Project-scoped: projects.<abs-cwd>.mcpServers
                if let Some(proj) = root
                    .get("projects")
                    .and_then(|p| p.get(cwd.to_string_lossy().as_ref()))
                {
                    push_source(
                        &mut out,
                        "claude-code (this project)",
                        &claude,
                        servers_from_json(proj),
                    );
                }
            }
        }
    }

    // --- Claude Code / generic project file: ./.mcp.json ---
    scan_json(&mut out, "claude-code (.mcp.json)", &cwd.join(".mcp.json"));

    // --- Codex: ~/.codex/config.toml ([mcp_servers.<name>]) ---
    if let Some(home) = home {
        let codex = home.join(".codex/config.toml");
        if let Ok(text) = std::fs::read_to_string(&codex) {
            push_source(&mut out, "codex", &codex, servers_from_codex_toml(&text));
        }
    }

    // --- Cursor: ~/.cursor/mcp.json (global) + ./.cursor/mcp.json (project) ---
    if let Some(home) = home {
        scan_json(&mut out, "cursor (global)", &home.join(".cursor/mcp.json"));
    }
    scan_json(&mut out, "cursor (project)", &cwd.join(".cursor/mcp.json"));

    // --- Claude Desktop: <config>/Claude/claude_desktop_config.json ---
    if let Some(cfg) = config_dir {
        scan_json(
            &mut out,
            "claude-desktop",
            &cfg.join("Claude/claude_desktop_config.json"),
        );
    }

    // --- Windsurf: ~/.codeium/windsurf/mcp_config.json ---
    if let Some(home) = home {
        scan_json(
            &mut out,
            "windsurf",
            &home.join(".codeium/windsurf/mcp_config.json"),
        );
    }

    // --- VS Code project: ./.vscode/mcp.json (uses the `servers` key) ---
    scan_json(&mut out, "vscode (project)", &cwd.join(".vscode/mcp.json"));

    out
}

/// Read a JSON MCP config and, if it has servers, push it as a source.
fn scan_json(out: &mut Vec<ImportSource>, label: &str, path: &Path) {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) {
            push_source(out, label, path, servers_from_json(&root));
        }
    }
}

fn push_source(out: &mut Vec<ImportSource>, label: &str, path: &Path, parsed: ParsedServers) {
    if !parsed.servers.is_empty() {
        out.push(ImportSource {
            label: label.to_string(),
            path: path.to_path_buf(),
            servers: parsed.servers,
            notes: parsed.notes,
            secrets: parsed.secrets,
        });
    }
}

/// Parse Codex's `~/.codex/config.toml` `[mcp_servers.<name>]` tables. Stdio entries use
/// `command`/`args`/`env`; http entries use `url`/`headers`. Secrets captured like the JSON path.
fn servers_from_codex_toml(text: &str) -> ParsedServers {
    let mut out = ParsedServers::default();
    let Ok(root) = text.parse::<toml::Table>() else {
        return out;
    };
    let Some(table) = root.get("mcp_servers").and_then(|v| v.as_table()) else {
        return out;
    };
    for (name, spec) in table {
        // Reuse the JSON parser by converting the TOML value to JSON (same field shapes).
        let json = serde_json::to_value(spec).unwrap_or(serde_json::Value::Null);
        server_from_json(name, &json, &mut out);
    }
    out
}

/// Read an existing `.forge/mcp.toml` into an [`McpConfig`], or the default if it's absent or
/// malformed. Used when merging newly-imported servers into a file that may already exist.
pub fn load_mcp_toml(path: &Path) -> McpConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// Serialize an [`McpConfig`] to a `.forge/mcp.toml` file (creating parent dirs). Secrets are
/// never present in `McpConfig`, so this is safe to write.
pub fn write_mcp_toml(path: &Path, config: &McpConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Write(e.to_string()))?;
    }
    let body = toml::to_string_pretty(config).map_err(|e| ConfigError::Write(e.to_string()))?;
    std::fs::write(path, body).map_err(|e| ConfigError::Write(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_empty_allows_all_declared() {
        let c = McpConfig::default();
        assert!(c.server_allowed("anything"));
        assert!(c.tool_allowed("gitlab__list_merge_requests"));
    }

    #[test]
    fn allowlist_restricts_when_set() {
        let c = McpConfig {
            allow: McpAllowlist {
                servers: vec!["gitlab".into()],
                tools: vec!["gitlab__get_mr_diff".into()],
            },
            ..Default::default()
        };
        assert!(c.server_allowed("gitlab"));
        assert!(!c.server_allowed("evil"));
        assert!(c.tool_allowed("gitlab__get_mr_diff"));
        assert!(!c.tool_allowed("gitlab__delete_repo"));
    }

    #[test]
    fn duplicate_server_names_rejected() {
        let stdio = || McpTransport::Stdio {
            command: "x".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let c = McpConfig {
            servers: vec![
                McpServerConfig {
                    name: "a".into(),
                    transport: stdio(),
                    auth: None,
                    enabled: true,
                },
                McpServerConfig {
                    name: "a".into(),
                    transport: stdio(),
                    auth: None,
                    enabled: true,
                },
            ],
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn token_resolves_from_env_first() {
        std::env::set_var("FORGE_TEST_MCP_TOKEN", "tok-123");
        let auth = McpAuth {
            token_env: Some("FORGE_TEST_MCP_TOKEN".into()),
            ..Default::default()
        };
        assert_eq!(resolve_token(&auth).as_deref(), Some("tok-123"));
        std::env::remove_var("FORGE_TEST_MCP_TOKEN");
        assert_eq!(resolve_token(&auth), None);
    }

    #[test]
    fn parses_mcp_toml_section() {
        let toml = r#"
call_timeout_secs = 30

[[servers]]
name = "gitlab"
[servers.transport]
type = "stdio"
command = "gitlab-mcp-server"
args = ["--read-only"]
[servers.transport.env]
GITLAB_URL = "https://gitlab.example.com"
[servers.auth]
token_env = "GITLAB_TOKEN"

[[servers]]
name = "docs"
[servers.transport]
type = "http"
url = "https://mcp.example.com/mcp"
"#;
        let c: McpConfig = toml::from_str(toml).unwrap();
        assert_eq!(c.call_timeout_secs, 30);
        assert_eq!(c.servers.len(), 2);
        assert_eq!(c.servers[0].name, "gitlab");
        assert_eq!(c.servers[0].transport_label(), "stdio");
        assert_eq!(c.servers[1].transport_label(), "http");
        match &c.servers[0].transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "gitlab-mcp-server");
                assert_eq!(args, &["--read-only"]);
                assert_eq!(env.get("GITLAB_URL").unwrap(), "https://gitlab.example.com");
            }
            _ => panic!("expected stdio"),
        }
        c.validate().unwrap();
    }

    #[test]
    fn codex_toml_servers_parse_and_infer_transport() {
        let toml = r#"
[mcp_servers.github]
command = "/home/x/.local/bin/claude-code-mcp"
args = ["github"]

[mcp_servers.remote]
url = "https://mcp.example.com/mcp"
[mcp_servers.remote.headers]
Authorization = "Bearer SECRET-TOKEN"
"#;
        let parsed = servers_from_codex_toml(toml);
        assert_eq!(parsed.servers.len(), 2);
        let gh = parsed.servers.iter().find(|s| s.name == "github").unwrap();
        assert_eq!(gh.transport_label(), "stdio");
        let remote = parsed.servers.iter().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.transport_label(), "http");
        // The Authorization token is captured (for the keyring) and referenced, never copied.
        assert_eq!(
            remote.auth.as_ref().unwrap().token_keyring.as_deref(),
            Some("mcp:remote")
        );
        // Captured with the `Bearer ` scheme stripped (Forge re-adds it when sending).
        assert_eq!(
            parsed.secrets.get("remote").map(String::as_str),
            Some("SECRET-TOKEN")
        );
        // Round-trip the parsed config: the secret must not appear in the serialized TOML.
        let cfg = McpConfig {
            servers: parsed.servers,
            ..Default::default()
        };
        let body = toml::to_string_pretty(&cfg).unwrap();
        assert!(!body.contains("SECRET-TOKEN"));
    }

    #[test]
    fn discovers_sources_across_clis() {
        // A fake HOME + cwd holding a Claude global config, a Codex config, and a project .mcp.json.
        let root = std::env::temp_dir().join(format!("forge-disco-{}", forge_types::new_id()));
        let home = root.join("home");
        let cwd = root.join("proj");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": { "helm": { "type": "http", "url": "https://h.example/mcp",
                    "headers": { "Authorization": "Bearer X" } } },
                "projects": { cwd.to_string_lossy(): { "mcpServers": {
                    "vectra": { "type": "stdio", "command": "npx", "args": ["-y", "vectra"] } } } }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.github]\ncommand = \"x\"\nargs = [\"github\"]\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".mcp.json"),
            serde_json::json!({ "mcpServers": {
                "local": { "command": "./srv", "args": [] } } })
            .to_string(),
        )
        .unwrap();

        // Inject the fake home directly — env overrides don't reach `directories` on Windows.
        let sources = discover_in(&cwd, Some(&home), Some(&home));

        let labels: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"claude-code (global)"), "{labels:?}");
        assert!(labels.contains(&"claude-code (this project)"), "{labels:?}");
        assert!(labels.contains(&"codex"), "{labels:?}");
        assert!(labels.contains(&"claude-code (.mcp.json)"), "{labels:?}");
        // The helm secret never lands in a parsed server.
        let helm = sources
            .iter()
            .flat_map(|s| &s.servers)
            .find(|s| s.name == "helm")
            .unwrap();
        match &helm.transport {
            McpTransport::Http { headers, .. } => assert!(!headers.contains_key("Authorization")),
            _ => panic!("http"),
        }
        // …but its token IS captured (for the keyring), Bearer-stripped.
        let helm_secret = sources
            .iter()
            .find_map(|s| s.secrets.get("helm"))
            .map(String::as_str);
        assert_eq!(helm_secret, Some("X"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stdio_server_with_two_secret_envs_keeps_one_and_flags_the_extra() {
        // A stdio server has ONE keyring slot. Two secret env vars used to silently last-wins; now
        // exactly one is stored and the extra is flagged (not dropped into plain env where it'd leak).
        let root = serde_json::json!({ "mcpServers": {
            "multi": {
                "command": "run-it",
                "env": { "API_KEY": "k1", "AUTH_TOKEN": "k2" }
            }
        }});
        let p = servers_from_json(&root);
        assert!(
            p.secrets.contains_key("multi"),
            "the first secret is still captured"
        );
        let server = p.servers.iter().find(|s| s.name == "multi").unwrap();
        // Neither secret leaks into plain env.
        if let McpTransport::Stdio { env, .. } = &server.transport {
            assert!(!env.contains_key("API_KEY"));
            assert!(!env.contains_key("AUTH_TOKEN"));
        } else {
            panic!("stdio");
        }
        assert!(
            p.notes.iter().any(|n| n.contains("IGNORING extra secret")),
            "the extra secret must be flagged loudly; notes: {:?}",
            p.notes
        );
    }

    #[test]
    fn import_captures_custom_header_vs_bearer() {
        let root = serde_json::json!({ "mcpServers": {
            "goog": { "url": "https://g/mcp", "headers": { "X-Goog-Api-Key": "GKEY" } },
            "bear": { "url": "https://b/mcp", "headers": { "Authorization": "Bearer BTOK" } }
        }});
        let p = servers_from_json(&root);
        let goog = p.servers.iter().find(|s| s.name == "goog").unwrap();
        // Custom header → the token rides verbatim in that header name.
        assert_eq!(
            goog.auth.as_ref().unwrap().header.as_deref(),
            Some("X-Goog-Api-Key")
        );
        assert_eq!(p.secrets.get("goog").map(String::as_str), Some("GKEY"));
        let bear = p.servers.iter().find(|s| s.name == "bear").unwrap();
        // Authorization → default Bearer (no custom header), token captured without the scheme.
        assert!(bear.auth.as_ref().unwrap().header.is_none());
        assert_eq!(p.secrets.get("bear").map(String::as_str), Some("BTOK"));
        // Nothing secret ends up in the serialized config.
        let body = toml::to_string_pretty(&McpConfig {
            servers: p.servers,
            ..Default::default()
        })
        .unwrap();
        assert!(!body.contains("GKEY") && !body.contains("BTOK"));
    }

    #[test]
    fn keyring_round_trip_store_resolve_delete() {
        // The "do it for me" mechanism: store a captured token, resolve it via token_keyring,
        // clean up. Tolerant of CI/headless boxes with no Secret Service.
        let key = format!("mcp:forge-selftest-{}", forge_types::new_id());
        if crate::store_secret(&key, "round-trip-token").is_err() {
            eprintln!("(skipping keyring round-trip — no OS keyring service available)");
            return;
        }
        let auth = McpAuth {
            token_keyring: Some(key.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_token(&auth).as_deref(), Some("round-trip-token"));
        // Clean up so the self-test leaves no residue in the keyring / file store.
        let _ = crate::secret_store::delete(&key);
    }

    #[test]
    fn import_mcp_json_translates_and_protects_secrets() {
        let dir = std::env::temp_dir().join(format!("forge-mcpimp-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = r#"{
          "mcpServers": {
            "gitlab": {
              "command": "gitlab-mcp",
              "args": ["--read-only"],
              "env": { "GITLAB_URL": "https://gl.example.com", "GITLAB_TOKEN": "glpat-SECRET" }
            },
            "docs": { "url": "https://mcp.example.com/mcp" }
          }
        }"#;
        let path = dir.join(".mcp.json");
        std::fs::write(&path, json).unwrap();

        let parsed = import_mcp_json(&path).unwrap();
        assert_eq!(parsed.servers.len(), 2);
        let gl = parsed.servers.iter().find(|s| s.name == "gitlab").unwrap();
        match &gl.transport {
            McpTransport::Stdio { env, .. } => {
                assert_eq!(env.get("GITLAB_URL").unwrap(), "https://gl.example.com");
                // The secret env value is NOT copied into config.
                assert!(!env.contains_key("GITLAB_TOKEN"));
            }
            _ => panic!("stdio"),
        }
        // The server references the env var by name + a keyring slot; the value is captured.
        let auth = gl.auth.as_ref().unwrap();
        assert_eq!(auth.token_env.as_deref(), Some("GITLAB_TOKEN"));
        assert_eq!(auth.token_keyring.as_deref(), Some("mcp:gitlab"));
        assert_eq!(
            parsed.secrets.get("gitlab").map(String::as_str),
            Some("glpat-SECRET")
        );

        // Round-trips through write_mcp_toml without leaking the secret.
        let out = dir.join("mcp.toml");
        let cfg = McpConfig {
            servers: parsed.servers,
            ..Default::default()
        };
        write_mcp_toml(&out, &cfg).unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(
            !written.contains("glpat-SECRET"),
            "no secret in written TOML"
        );
        let reparsed: McpConfig = toml::from_str(&written).unwrap();
        assert_eq!(reparsed.servers.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
