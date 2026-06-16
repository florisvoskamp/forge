//! Forge as an MCP **client** (docs/features/mcp-client.md). [`McpManager`] connects to the
//! external servers declared in `[mcp]` config — over stdio (child process) or HTTP/SSE — using
//! the official `rmcp` SDK, discovers their tools/resources/prompts, and surfaces them to the
//! agent loop through Forge's existing tool-calling + permission spine.
//!
//! Integration points (`forge-core` on the direct path, `forge mcp-serve` on the CLI-bridge path):
//! - [`McpManager::advertised_specs`] feeds the model's tool list — only the fixed **meta-tools**
//!   (`mcp_search_tools` to find a tool, `mcp_call` to run it, plus resources/prompt). A server's
//!   own tools are reached *through* `mcp_call`, never advertised individually: this keeps the
//!   per-turn tool list tiny for a 300-tool server AND works on the CLI bridge, where the model's
//!   tool list is fixed once per turn (so a dynamically-"exposed" tool could never become callable
//!   mid-turn).
//! - [`McpManager::knows_tool`] + [`McpManager::side_effect_of`] + [`McpManager::call`] are
//!   driven from `Session::invoke_tool` (and `ForgeMcp::call_tool` on the bridge), behind the
//!   permission broker. Every server call is `SideEffect::External` (untrusted) and gated.
//!
//! Security: servers are untrusted by default. The allowlist gates which servers/tools are
//! reachable, deferred loading keeps hostile tool descriptions out of context until surfaced,
//! and tokens resolve from env/keyring only (ADR-0007) — never logged, never in TOML.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use forge_config::McpConfig;
use forge_types::{McpServerLine, SideEffect};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, ReadResourceRequestParams,
    ResourceContents,
};
use rmcp::service::{RoleClient, RunningService};
use serde_json::Value;

mod transport;

/// Meta-tool names (the deferred-loading + resource/prompt surface). Mirrors the
/// `ToolSearch`-style mechanism the harness Forge itself runs under.
pub const MCP_SEARCH_TOOLS: &str = "mcp_search_tools";
pub const MCP_CALL: &str = "mcp_call";
pub const MCP_LIST_RESOURCES: &str = "mcp_list_resources";
pub const MCP_READ_RESOURCE: &str = "mcp_read_resource";
pub const MCP_GET_PROMPT: &str = "mcp_get_prompt";

/// Server results larger than this are truncated before entering the model's context (with a
/// notice). Mirrors the size-guarding the built-in tools apply.
const MAX_RESULT_CHARS: usize = 16_000;

/// A neutral tool spec (name/description/JSON-schema). `forge-core` maps it onto its `ToolSpec`
/// so `forge-mcp` need not depend on `forge-provider`.
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// The result of running an MCP (meta-)tool: the text to feed the model + whether it succeeded.
#[derive(Debug, Clone)]
pub struct McpCallOutcome {
    pub text: String,
    pub ok: bool,
}

impl McpCallOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: true,
        }
    }
    fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerStatus {
    Connected,
    Reconnecting,
    Unauthorized,
    /// A call exceeded the soft latency threshold; still usable.
    Slow,
    Failed(String),
    Disabled,
}

impl ServerStatus {
    fn word(&self) -> &'static str {
        match self {
            ServerStatus::Connected => "connected",
            ServerStatus::Reconnecting => "reconnecting",
            ServerStatus::Unauthorized => "unauthorized",
            ServerStatus::Slow => "slow",
            ServerStatus::Failed(_) => "failed",
            ServerStatus::Disabled => "disabled",
        }
    }
}

/// A discovered tool, namespaced so two servers exposing `search` can't collide.
#[derive(Debug, Clone)]
struct DiscoveredTool {
    raw_name: String,
    qualified: String,
    description: String,
    schema: Value,
}

#[derive(Debug, Clone)]
struct DiscoveredResource {
    uri: String,
    name: String,
    mime: Option<String>,
}

#[derive(Debug, Clone)]
struct DiscoveredPrompt {
    name: String,
    description: String,
}

/// One connected (or failed) server. `service` owns the connection lifecycle; `peer` is a cheap
/// clone used for calls so the manager never holds its lock across an `.await`.
struct Connection {
    name: String,
    status: ServerStatus,
    transport_label: &'static str,
    peer: Option<rmcp::service::Peer<RoleClient>>,
    service: Option<RunningService<RoleClient, ()>>,
    tools: Vec<DiscoveredTool>,
    resources: Vec<DiscoveredResource>,
    prompts: Vec<DiscoveredPrompt>,
    reconnect_attempts: usize,
}

/// Connects to and drives a set of external MCP servers. Cheap to hold in an `Arc`; all mutable
/// state is behind short-lived mutexes (never locked across an `.await`).
pub struct McpManager {
    conns: Mutex<HashMap<String, Connection>>,
    config: McpConfig,
    call_timeout: Duration,
    connect_timeout: Duration,
}

impl McpManager {
    fn empty(config: &McpConfig) -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            config: config.clone(),
            call_timeout: Duration::from_secs(config.call_timeout_secs.max(1)),
            connect_timeout: Duration::from_secs(config.connect_timeout_secs.max(1)),
        }
    }

    /// Connect to every enabled + allowlisted server concurrently, isolating failures: a server
    /// that can't connect lands `failed` with a reason but never blocks the others or the session.
    /// Blocking — the returned manager is fully connected. (`mcp-serve` instead uses
    /// [`connecting`](Self::connecting) + a background [`connect_active`](Self::connect_active) so
    /// it never stalls the bridge's tool advertisement on a slow external server.)
    pub async fn connect_all(config: &McpConfig) -> Self {
        let mgr = Self::empty(config);
        mgr.connect_active().await;
        mgr
    }

    /// Construct the manager with every active server pre-marked `Reconnecting` (a connect is in
    /// flight) WITHOUT awaiting any network I/O. Crucially `is_empty()` is then false, so the MCP
    /// meta-tools (`mcp_search_tools`/`mcp_call`/…) are advertised IMMEDIATELY and a slow external
    /// server (e.g. an OAuth one) can't delay the rest of the tool surface. Pair with a background
    /// [`connect_active`](Self::connect_active); the first `mcp_call` lazily connects on demand.
    pub fn connecting(config: &McpConfig) -> Self {
        let mgr = Self::empty(config);
        {
            let mut conns = mgr.conns.lock().unwrap();
            for s in config.active_servers() {
                conns.insert(
                    s.name.clone(),
                    Connection {
                        name: s.name.clone(),
                        status: ServerStatus::Reconnecting,
                        transport_label: s.transport_label(),
                        peer: None,
                        service: None,
                        tools: vec![],
                        resources: vec![],
                        prompts: vec![],
                        reconnect_attempts: 0,
                    },
                );
            }
        }
        mgr
    }

    /// Connect every active server concurrently (isolating failures) and surface declared-inactive
    /// ones, overwriting any placeholder entries. Shared by the blocking [`connect_all`] and the
    /// background path in `mcp-serve` (after [`connecting`](Self::connecting)).
    pub async fn connect_active(&self) {
        let connect_timeout = self.connect_timeout;
        let servers: Vec<_> = self.config.active_servers().cloned().collect();
        let results = futures::future::join_all(servers.into_iter().map(|s| async move {
            let label = s.transport_label();
            match tokio::time::timeout(connect_timeout, transport::serve(&s)).await {
                Ok(Ok(service)) => (s.name.clone(), label, Ok(service)),
                Ok(Err(e)) => (s.name.clone(), label, Err(e)),
                Err(_) => (
                    s.name.clone(),
                    label,
                    Err(format!(
                        "connect timed out after {}s",
                        connect_timeout.as_secs()
                    )),
                ),
            }
        }))
        .await;

        for (name, label, res) in results {
            match res {
                Ok(service) => self.add_established(&name, label, service).await,
                Err(reason) => {
                    tracing::warn!("mcp: server '{name}' failed to connect: {reason}");
                    self.conns.lock().unwrap().insert(
                        name.clone(),
                        Connection {
                            name,
                            status: ServerStatus::Failed(reason),
                            transport_label: label,
                            peer: None,
                            service: None,
                            tools: vec![],
                            resources: vec![],
                            prompts: vec![],
                            reconnect_attempts: 0,
                        },
                    );
                }
            }
        }
        // Surface declared-but-inactive servers (disabled, or excluded by the allowlist) so the
        // user sees them in `forge mcp` rather than wondering why they're silent.
        {
            let mut conns = self.conns.lock().unwrap();
            for s in &self.config.servers {
                if !conns.contains_key(&s.name) {
                    conns.insert(
                        s.name.clone(),
                        Connection {
                            name: s.name.clone(),
                            status: ServerStatus::Disabled,
                            transport_label: s.transport_label(),
                            peer: None,
                            service: None,
                            tools: vec![],
                            resources: vec![],
                            prompts: vec![],
                            reconnect_attempts: 0,
                        },
                    );
                }
            }
        }
    }

    /// Given an initialized client connection, list its tools/resources/prompts, namespace them,
    /// and store it as a live server. Shared by [`connect_all`] and the in-process tests.
    async fn add_established(
        &self,
        name: &str,
        transport_label: &'static str,
        service: RunningService<RoleClient, ()>,
    ) {
        let peer = service.peer().clone();
        let tools = peer
            .list_all_tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| DiscoveredTool {
                qualified: format!("{name}__{}", t.name),
                raw_name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                schema: Value::Object((*t.input_schema).clone()),
            })
            .collect();
        let resources = peer
            .list_all_resources()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| DiscoveredResource {
                uri: r.uri.clone(),
                name: r.name.clone(),
                mime: r.mime_type.clone(),
            })
            .collect();
        let prompts = peer
            .list_all_prompts()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| DiscoveredPrompt {
                name: p.name.clone(),
                description: p.description.clone().unwrap_or_default(),
            })
            .collect();

        self.conns.lock().unwrap().insert(
            name.to_string(),
            Connection {
                name: name.to_string(),
                status: ServerStatus::Connected,
                transport_label,
                peer: Some(peer),
                service: Some(service),
                tools,
                resources,
                prompts,
                reconnect_attempts: 0,
            },
        );
    }

    /// No servers connected/declared — the whole MCP path is inert.
    pub fn is_empty(&self) -> bool {
        self.conns.lock().unwrap().is_empty()
    }

    /// The tools advertised to the model: just the fixed meta-tools (search / call / resources /
    /// prompt). Server tools are reached *through* `mcp_call`, never advertised individually — so
    /// the per-turn tool list stays tiny regardless of how many tools a server has (a 313-tool
    /// server adds nothing here), and it works identically on the direct and CLI-bridge paths
    /// (the bridge fixes its tool list once per turn, so a dynamically-"exposed" tool could never
    /// become callable mid-turn — `mcp_call` sidesteps that entirely). Empty when no servers.
    pub fn advertised_specs(&self) -> Vec<McpToolSpec> {
        if self.conns.lock().unwrap().is_empty() {
            return vec![];
        }
        meta_specs()
    }

    /// Whether `name` is an MCP meta-tool — i.e. core should route it here rather than to the
    /// built-in registry. (Server tools are invoked via `mcp_call`, never by their own name.)
    pub fn knows_tool(&self, name: &str) -> bool {
        !self.conns.lock().unwrap().is_empty() && is_meta_tool(name)
    }

    /// The permission class for a meta-tool. Local catalog reads (`mcp_search_tools`,
    /// `mcp_list_resources`) are `ReadOnly`; anything that hits a server (`mcp_call`,
    /// `mcp_read_resource`, `mcp_get_prompt`) is `External` (untrusted, gated).
    pub fn side_effect_of(&self, name: &str) -> SideEffect {
        match name {
            MCP_SEARCH_TOOLS | MCP_LIST_RESOURCES => SideEffect::ReadOnly,
            _ => SideEffect::External,
        }
    }

    /// Run an MCP meta-tool or a qualified server tool. Never panics; transport/timeout failures
    /// come back as `ok=false` tool errors so the turn continues.
    pub async fn call(&self, name: &str, args: &Value) -> McpCallOutcome {
        match name {
            MCP_SEARCH_TOOLS => self.search_tools(args),
            MCP_CALL => self.mcp_call(args).await,
            MCP_LIST_RESOURCES => self.list_resources(args),
            MCP_READ_RESOURCE => self.read_resource(args).await,
            MCP_GET_PROMPT => self.get_prompt(args).await,
            // Defensive: a direct qualified-name call still works (e.g. legacy callers).
            qualified => self.call_server_tool(qualified, args).await,
        }
    }

    /// `mcp_call { name: "server__tool", arguments: {...} }` — the universal invoker. The model
    /// finds a tool with `mcp_search_tools`, then calls it here by qualified name. This is the
    /// single mechanism that works on every path (no per-tool advertising, no dynamic tool list).
    async fn mcp_call(&self, args: &Value) -> McpCallOutcome {
        let name = args
            .get("name")
            .or_else(|| args.get("qualified_name"))
            .or_else(|| args.get("tool"))
            .and_then(Value::as_str);
        let Some(name) = name else {
            return McpCallOutcome::err("mcp_call: expected string 'name' (a server__tool name)");
        };
        // `arguments` may be an object, or absent (no-arg tool).
        let inner = args
            .get("arguments")
            .or_else(|| args.get("args"))
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        self.call_server_tool(name, &inner).await
    }

    // ---- meta-tools (local catalog) ----

    fn search_tools(&self, args: &Value) -> McpCallOutcome {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let server_filter = args.get("server").and_then(Value::as_str);
        let terms: Vec<&str> = query.split_whitespace().collect();
        let conns = self.conns.lock().unwrap();
        #[allow(clippy::type_complexity)]
        let mut scored: Vec<(i64, String, String, String)> = Vec::new();
        for conn in conns.values() {
            if let Some(sf) = server_filter {
                if conn.name != sf {
                    continue;
                }
            }
            for t in &conn.tools {
                let hay = format!("{} {}", t.qualified, t.description).to_lowercase();
                let score: i64 = terms
                    .iter()
                    .map(|term| if hay.contains(term) { 1 } else { 0 })
                    .sum();
                if terms.is_empty() || score > 0 {
                    scored.push((
                        score,
                        t.qualified.clone(),
                        one_line(&t.description),
                        schema_hint(&t.schema),
                    ));
                }
            }
        }
        drop(conns);
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(15);
        if scored.is_empty() {
            return McpCallOutcome::ok("no matching MCP tools".to_string());
        }
        let mut out = format!("{} matching MCP tool(s):\n", scored.len());
        for (_, name, desc, hint) in &scored {
            out.push_str(&format!("  {name} — {desc}\n      args: {hint}\n"));
        }
        out.push_str(
            "\nTo run one, call mcp_call { \"name\": \"server__tool\", \"arguments\": { … } }.",
        );
        McpCallOutcome::ok(out)
    }

    fn list_resources(&self, args: &Value) -> McpCallOutcome {
        let server_filter = args.get("server").and_then(Value::as_str);
        let conns = self.conns.lock().unwrap();
        let mut out = String::new();
        let mut n = 0;
        for conn in conns.values() {
            if let Some(sf) = server_filter {
                if conn.name != sf {
                    continue;
                }
            }
            for r in &conn.resources {
                let mime = r.mime.as_deref().unwrap_or("?");
                out.push_str(&format!(
                    "  [{}] {} ({mime}) — {}\n",
                    conn.name, r.uri, r.name
                ));
                n += 1;
            }
        }
        if n == 0 {
            return McpCallOutcome::ok("no MCP resources available".to_string());
        }
        McpCallOutcome::ok(format!("{n} MCP resource(s):\n{out}"))
    }

    // ---- meta-tools (server round-trip) ----

    async fn read_resource(&self, args: &Value) -> McpCallOutcome {
        let Some(server) = args.get("server").and_then(Value::as_str) else {
            return McpCallOutcome::err("expected string 'server'");
        };
        let Some(uri) = args.get("uri").and_then(Value::as_str) else {
            return McpCallOutcome::err("expected string 'uri'");
        };
        let Some(peer) = self.peer_for(server) else {
            return McpCallOutcome::err(format!("mcp: server '{server}' unavailable"));
        };
        let params = ReadResourceRequestParams::new(uri);
        match tokio::time::timeout(self.call_timeout, peer.read_resource(params)).await {
            Ok(Ok(res)) => {
                let text = res
                    .contents
                    .iter()
                    .map(resource_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                McpCallOutcome::ok(truncate(&text))
            }
            Ok(Err(e)) => self.classify_call_error(server, e),
            Err(_) => self.timed_out(server),
        }
    }

    async fn get_prompt(&self, args: &Value) -> McpCallOutcome {
        let Some(server) = args.get("server").and_then(Value::as_str) else {
            return McpCallOutcome::err("expected string 'server'");
        };
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return McpCallOutcome::err("expected string 'name'");
        };
        let arguments = args.get("arguments").and_then(|v| v.as_object()).cloned();
        // Validate against the discovered prompt catalog; on a miss, list what's available.
        {
            let conns = self.conns.lock().unwrap();
            if let Some(conn) = conns.get(server) {
                if !conn.prompts.iter().any(|p| p.name == name) {
                    let avail = conn
                        .prompts
                        .iter()
                        .map(|p| format!("  {} — {}", p.name, one_line(&p.description)))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return McpCallOutcome::err(if avail.is_empty() {
                        format!("mcp: server '{server}' exposes no prompts")
                    } else {
                        format!("mcp: no prompt '{name}' on '{server}'. Available:\n{avail}")
                    });
                }
            }
        }
        let Some(peer) = self.peer_for(server) else {
            return McpCallOutcome::err(format!("mcp: server '{server}' unavailable"));
        };
        let mut params = GetPromptRequestParams::new(name);
        params.arguments = arguments;
        match tokio::time::timeout(self.call_timeout, peer.get_prompt(params)).await {
            Ok(Ok(res)) => {
                let text = res
                    .messages
                    .iter()
                    .map(prompt_message_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                McpCallOutcome::ok(truncate(&text))
            }
            Ok(Err(e)) => self.classify_call_error(server, e),
            Err(_) => self.timed_out(server),
        }
    }

    // ---- the real thing: a server tool call ----

    async fn call_server_tool(&self, qualified: &str, args: &Value) -> McpCallOutcome {
        // Resolve qualified -> (server, raw name), re-fetching nothing: catalog is authoritative.
        let resolved = {
            let conns = self.conns.lock().unwrap();
            conns.values().find_map(|c| {
                c.tools
                    .iter()
                    .find(|t| t.qualified == qualified)
                    .map(|t| (c.name.clone(), t.raw_name.clone()))
            })
        };
        let Some((server, raw_name)) = resolved else {
            return McpCallOutcome::err(format!(
                "mcp: tool '{qualified}' no longer exists (server updated its tools)"
            ));
        };
        if !self.config.tool_allowed(qualified) {
            return McpCallOutcome::err(format!("mcp: '{qualified}' denied by policy"));
        }
        let peer = match self.peer_for(&server) {
            Some(p) => p,
            None => match self.reconnect(&server).await {
                Some(p) => p,
                None => {
                    return McpCallOutcome::err(format!("mcp: server '{server}' unavailable"));
                }
            },
        };
        let arguments = args.as_object().cloned();
        let mut params = CallToolRequestParams::new(raw_name);
        params.arguments = arguments;
        match tokio::time::timeout(self.call_timeout, peer.call_tool(params)).await {
            Ok(Ok(result)) => {
                self.mark(&server, ServerStatus::Connected);
                tool_result_to_outcome(result)
            }
            Ok(Err(e)) => self.classify_call_error(&server, e),
            Err(_) => self.timed_out(&server),
        }
    }

    // ---- connection helpers ----

    fn peer_for(&self, server: &str) -> Option<rmcp::service::Peer<RoleClient>> {
        self.conns
            .lock()
            .unwrap()
            .get(server)
            .and_then(|c| c.peer.clone())
    }

    fn mark(&self, server: &str, status: ServerStatus) {
        if let Some(c) = self.conns.lock().unwrap().get_mut(server) {
            // Don't overwrite a hard Failed with a transient Slow.
            c.status = status;
        }
    }

    /// Classify a call error: HTTP 401/403 → `unauthorized` (no retry loop); anything else is
    /// treated as a dropped connection → mark `reconnecting` (a later call lazily reconnects).
    fn classify_call_error(&self, server: &str, e: impl std::fmt::Display) -> McpCallOutcome {
        let msg = e.to_string();
        let lc = msg.to_lowercase();
        if lc.contains("401") || lc.contains("403") || lc.contains("unauthor") {
            self.mark(server, ServerStatus::Unauthorized);
            return McpCallOutcome::err(format!(
                "mcp: {server} auth failed (token expired?) — see `forge mcp`"
            ));
        }
        self.mark(server, ServerStatus::Reconnecting);
        McpCallOutcome::err(format!("mcp: {server} disconnected ({msg})"))
    }

    fn timed_out(&self, server: &str) -> McpCallOutcome {
        self.mark(server, ServerStatus::Slow);
        McpCallOutcome::err(format!(
            "mcp: {server} timed out after {}s",
            self.call_timeout.as_secs()
        ))
    }

    /// Lazily reconnect a dropped stdio/HTTP server, bounded by `max_reconnect_attempts`. On
    /// success re-runs discovery (picking up schema drift) and re-applies exposure; returns the
    /// fresh peer. On exhaustion marks the server `failed` and withdraws its tools.
    async fn reconnect(&self, server: &str) -> Option<rmcp::service::Peer<RoleClient>> {
        let cfg = {
            let conns = self.conns.lock().unwrap();
            let c = conns.get(server)?;
            if c.reconnect_attempts >= self.config.max_reconnect_attempts {
                return None;
            }
            self.config
                .servers
                .iter()
                .find(|s| s.name == server)
                .cloned()?
        };
        // Backoff grows with the attempt count already recorded.
        let attempt = self
            .conns
            .lock()
            .unwrap()
            .get(server)
            .map(|c| c.reconnect_attempts)
            .unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 + 1))).await;
        let label = cfg.transport_label();
        match tokio::time::timeout(self.connect_timeout, transport::serve(&cfg)).await {
            Ok(Ok(service)) => {
                self.add_established(server, label, service).await;
                self.peer_for(server)
            }
            _ => {
                if let Some(c) = self.conns.lock().unwrap().get_mut(server) {
                    c.reconnect_attempts += 1;
                    if c.reconnect_attempts >= self.config.max_reconnect_attempts {
                        c.status = ServerStatus::Failed("reconnect attempts exhausted".into());
                        c.tools.clear();
                        c.peer = None;
                    }
                }
                None
            }
        }
    }

    // ---- status surfacing ----

    /// One [`McpServerLine`] per declared server (connected or not), for `forge mcp` / `/mcp`.
    pub fn status_lines(&self) -> Vec<McpServerLine> {
        let conns = self.conns.lock().unwrap();
        let mut lines: Vec<McpServerLine> = conns
            .values()
            .map(|c| McpServerLine {
                name: c.name.clone(),
                status: c.status.word().to_string(),
                transport: c.transport_label.to_string(),
                tools: c.tools.len(),
                resources: c.resources.len(),
                prompts: c.prompts.len(),
                detail: match &c.status {
                    ServerStatus::Failed(r) => Some(r.clone()),
                    ServerStatus::Reconnecting => Some(format!(
                        "attempt {}/{}",
                        c.reconnect_attempts, self.config.max_reconnect_attempts
                    )),
                    ServerStatus::Unauthorized => Some("token expired?".into()),
                    _ => None,
                },
            })
            .collect();
        lines.sort_by(|a, b| a.name.cmp(&b.name));
        lines
    }

    /// `(qualified_name, one-line description)` for a server's full discovered tool list
    /// (`forge mcp --tools <server>`).
    pub fn tool_lines(&self, server: &str) -> Vec<(String, String)> {
        let conns = self.conns.lock().unwrap();
        conns
            .get(server)
            .map(|c| {
                c.tools
                    .iter()
                    .map(|t| (t.qualified.clone(), one_line(&t.description)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Close all connections (kill stdio children / close HTTP streams). Best-effort.
    pub async fn shutdown(&self) {
        let services: Vec<RunningService<RoleClient, ()>> = {
            let mut conns = self.conns.lock().unwrap();
            conns
                .values_mut()
                .filter_map(|c| c.service.take())
                .collect()
        };
        for s in services {
            let _ = s.cancel().await;
        }
    }
}

// ---- free helpers ----

fn is_meta_tool(name: &str) -> bool {
    matches!(
        name,
        MCP_SEARCH_TOOLS | MCP_CALL | MCP_LIST_RESOURCES | MCP_READ_RESOURCE | MCP_GET_PROMPT
    )
}

/// A compact one-line parameter hint from a JSON-schema object: `query:string!, count:integer`
/// (`!` marks required). Lets `mcp_search_tools` tell the model how to fill `mcp_call`'s
/// `arguments` without dumping the full schema for every match.
fn schema_hint(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return "(none)".to_string();
    };
    if props.is_empty() {
        return "(none)".to_string();
    }
    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut parts: Vec<String> = props
        .iter()
        .map(|(k, v)| {
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("any");
            let req = if required.contains(k.as_str()) {
                "!"
            } else {
                ""
            };
            format!("{k}:{ty}{req}")
        })
        .collect();
    parts.sort();
    parts.join(", ")
}

/// The MCP meta-tools, always advertised when at least one server is configured.
fn meta_specs() -> Vec<McpToolSpec> {
    vec![
        McpToolSpec {
            name: MCP_SEARCH_TOOLS.into(),
            description: "Find tools on the connected MCP servers (e.g. the helm server). Returns \
                matching qualified `server__tool` names, descriptions, and an args hint. ALWAYS \
                use this first to locate the right tool, then invoke it with `mcp_call`. Optional \
                `server` restricts to one server."
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "what you're looking for, e.g. 'net worth'" },
                    "server": { "type": "string", "description": "optional: restrict to one server" }
                },
                "required": ["query"]
            }),
        },
        McpToolSpec {
            name: MCP_CALL.into(),
            description: "Invoke a tool on a connected MCP server. Pass the qualified `name` \
                (`server__tool`, from mcp_search_tools) and an `arguments` object matching that \
                tool's schema. This is how you actually run any MCP server tool (net worth, \
                merge-request review, etc.) — there is no separate step."
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "qualified server__tool name" },
                    "arguments": { "type": "object", "description": "arguments for the tool (may be omitted if none)" }
                },
                "required": ["name"]
            }),
        },
        McpToolSpec {
            name: MCP_LIST_RESOURCES.into(),
            description: "List resources offered by connected MCP servers (uri + name + type). \
                Optional `server` filters to one server."
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": { "server": { "type": "string" } }
            }),
        },
        McpToolSpec {
            name: MCP_READ_RESOURCE.into(),
            description: "Read an MCP resource's contents by `server` + `uri`.".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"]
            }),
        },
        McpToolSpec {
            name: MCP_GET_PROMPT.into(),
            description: "Render a server-provided MCP prompt by `server` + `name` (+ optional \
                `arguments` object)."
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "name": { "type": "string" },
                    "arguments": { "type": "object" }
                },
                "required": ["server", "name"]
            }),
        },
    ]
}

fn tool_result_to_outcome(result: CallToolResult) -> McpCallOutcome {
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    let text = if text.is_empty() {
        "(no textual content)".to_string()
    } else {
        truncate(&text)
    };
    // An MCP `isError` payload is a tool error, not a successful result.
    if result.is_error == Some(true) {
        McpCallOutcome::err(text)
    } else {
        McpCallOutcome::ok(text)
    }
}

fn resource_text(c: &ResourceContents) -> String {
    match c {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        ResourceContents::BlobResourceContents { uri, mime_type, .. } => format!(
            "(binary resource {uri}, {})",
            mime_type.as_deref().unwrap_or("application/octet-stream")
        ),
    }
}

fn prompt_message_text(m: &rmcp::model::PromptMessage) -> String {
    use rmcp::model::PromptMessageContent;
    match &m.content {
        PromptMessageContent::Text { text } => text.clone(),
        _ => String::new(),
    }
}

fn one_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(119).collect::<String>())
    } else {
        line.to_string()
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_RESULT_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_RESULT_CHARS).collect();
    format!(
        "{head}\n…[truncated {} chars]",
        s.chars().count() - MAX_RESULT_CHARS
    )
}

/// Test/wiring support — not part of the stable API. A tiny in-process MCP server (two tools:
/// `echo`, `boom`) served over a duplex stream, plus [`testsupport::manager_with_echo`] which
/// returns an [`McpManager`] connected to it. Lets downstream crates (forge-core, forge-cli)
/// exercise their MCP integration against a real connection without spawning a child process.
#[doc(hidden)]
pub mod testsupport {
    use super::*;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    };
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
    use std::sync::Arc;

    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            let mut info = ServerInfo::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info
        }
        async fn list_tools(
            &self,
            _req: Option<PaginatedRequestParams>,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            let schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } }
            }))
            .unwrap();
            Ok(ListToolsResult {
                tools: vec![
                    Tool::new(
                        "echo",
                        "Echo back the msg argument",
                        Arc::new(schema.clone()),
                    ),
                    Tool::new("boom", "Always fails", Arc::new(schema)),
                ],
                next_cursor: None,
                meta: None,
            })
        }
        async fn call_tool(
            &self,
            req: CallToolRequestParams,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            match req.name.as_ref() {
                "echo" => {
                    let msg = req
                        .arguments
                        .as_ref()
                        .and_then(|a| a.get("msg"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    Ok(CallToolResult::success(vec![Content::text(format!(
                        "echo: {msg}"
                    ))]))
                }
                "boom" => Ok(CallToolResult::error(vec![Content::text("kaboom")])),
                other => Ok(CallToolResult::error(vec![Content::text(format!(
                    "unknown tool {other}"
                ))])),
            }
        }
    }

    /// An [`McpManager`] connected (in-process) to a server named `test` exposing `echo`+`boom`.
    pub async fn manager_with_echo(config: &McpConfig) -> McpManager {
        let (client_io, server_io) = tokio::io::duplex(8 * 1024);
        tokio::spawn(async move {
            if let Ok(server) = EchoServer.serve(server_io).await {
                let _ = server.waiting().await;
            }
        });
        let client = ().serve(client_io).await.expect("client connects");
        let mgr = McpManager::empty(config);
        mgr.add_established("test", "stdio", client).await;
        mgr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn manager_with_test_server(config: McpConfig) -> McpManager {
        testsupport::manager_with_echo(&config).await
    }

    #[tokio::test]
    async fn connects_discovers_and_namespaces_tools() {
        let mgr = manager_with_test_server(McpConfig::default()).await;
        let lines = mgr.status_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].status, "connected");
        assert_eq!(lines[0].tools, 2);
        let tools = mgr.tool_lines("test");
        assert!(tools.iter().any(|(q, _)| q == "test__echo"));
        assert!(tools.iter().any(|(q, _)| q == "test__boom"));
        assert!(mgr.knows_tool(MCP_SEARCH_TOOLS));
        assert!(mgr.knows_tool(MCP_CALL));
        // Server tools are reached via mcp_call, never routed by their own name.
        assert!(!mgr.knows_tool("test__echo"));
    }

    #[tokio::test]
    async fn only_meta_tools_advertised_regardless_of_server_size() {
        // The advertised set is the fixed meta-tools — never the server's own tools. This is what
        // keeps a 313-tool server from flooding the model AND what makes it work on the bridge.
        let mgr = manager_with_test_server(McpConfig::default()).await;
        let advertised = mgr.advertised_specs();
        assert!(
            advertised.iter().all(|s| s.name.starts_with("mcp_")),
            "only meta-tools advertised"
        );
        assert_eq!(advertised.len(), 5, "the five meta-tools");
        assert!(advertised.iter().any(|s| s.name == MCP_CALL));
        assert!(advertised.iter().all(|s| s.name != "test__echo"));

        // Search finds the tool + an args hint without advertising it.
        let found = mgr
            .call(MCP_SEARCH_TOOLS, &serde_json::json!({"query": "echo"}))
            .await;
        assert!(found.ok && found.text.contains("test__echo"));
        assert!(
            found.text.contains("msg:string"),
            "args hint present: {}",
            found.text
        );
    }

    #[tokio::test]
    async fn mcp_call_invokes_a_server_tool_and_surfaces_errors() {
        let mgr = manager_with_test_server(McpConfig::default()).await;
        // The universal path: mcp_call { name, arguments }.
        let ok = mgr
            .call(
                MCP_CALL,
                &serde_json::json!({"name": "test__echo", "arguments": {"msg": "hi"}}),
            )
            .await;
        assert!(ok.ok);
        assert_eq!(ok.text, "echo: hi");

        // An isError payload becomes ok=false.
        let bad = mgr
            .call(MCP_CALL, &serde_json::json!({"name": "test__boom"}))
            .await;
        assert!(!bad.ok);
        assert!(bad.text.contains("kaboom"));

        // A vanished tool is a clean error, not a hang/panic.
        let gone = mgr
            .call(MCP_CALL, &serde_json::json!({"name": "test__missing"}))
            .await;
        assert!(!gone.ok);
        assert!(gone.text.contains("no longer exists"));
    }

    #[tokio::test]
    async fn external_side_effect_classification() {
        let mgr = manager_with_test_server(McpConfig::default()).await;
        assert_eq!(mgr.side_effect_of(MCP_CALL), SideEffect::External);
        assert_eq!(mgr.side_effect_of(MCP_READ_RESOURCE), SideEffect::External);
        assert_eq!(mgr.side_effect_of(MCP_SEARCH_TOOLS), SideEffect::ReadOnly);
        assert_eq!(mgr.side_effect_of(MCP_LIST_RESOURCES), SideEffect::ReadOnly);
    }

    #[tokio::test]
    async fn allowlist_blocks_calling_excluded_tools() {
        let config = McpConfig {
            allow: forge_config::McpAllowlist {
                servers: vec!["test".into()],
                tools: vec!["test__echo".into()], // boom excluded
            },
            ..Default::default()
        };
        let mgr = manager_with_test_server(config).await;
        // echo is allowlisted → callable via mcp_call.
        let ok = mgr
            .call(
                MCP_CALL,
                &serde_json::json!({"name": "test__echo", "arguments": {"msg": "x"}}),
            )
            .await;
        assert!(ok.ok, "{}", ok.text);
        // boom is excluded → denied by policy.
        let call = mgr
            .call(MCP_CALL, &serde_json::json!({"name": "test__boom"}))
            .await;
        assert!(!call.ok && call.text.contains("denied by policy"));
    }

    #[tokio::test]
    async fn empty_manager_is_inert() {
        let mgr = McpManager::connect_all(&McpConfig::default()).await;
        assert!(mgr.is_empty());
        assert!(mgr.advertised_specs().is_empty());
        assert!(
            !mgr.knows_tool(MCP_SEARCH_TOOLS),
            "no meta-tools without servers"
        );
    }

    #[test]
    fn connecting_advertises_meta_tools_without_any_network_io() {
        // The fix for "mcp-serve stalls behind a slow external server": `connecting` must make the
        // meta-tools available IMMEDIATELY (no await, no connection) so the bridge serves its tool
        // list instantly; `connect_active` fills in the real status in the background.
        let config = McpConfig {
            servers: vec![forge_config::McpServerConfig {
                name: "blackhole".into(),
                transport: forge_config::McpTransport::Http {
                    url: "http://10.255.255.1:8080/mcp".into(),
                    headers: Default::default(),
                },
                auth: None,
                enabled: true,
            }],
            ..Default::default()
        };
        let mgr = McpManager::connecting(&config);
        assert!(!mgr.is_empty(), "declared server is present immediately");
        assert!(
            !mgr.advertised_specs().is_empty(),
            "meta-tools advertised before any connection completes"
        );
        assert!(mgr.knows_tool(MCP_CALL), "mcp_call routable immediately");
    }
}
