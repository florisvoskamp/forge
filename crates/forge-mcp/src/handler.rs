//! The rmcp **client handler** Forge presents to every connected MCP server.
//!
//! Until now the client handler was the unit type `()`, which advertises no capabilities and
//! serves no server-initiated requests. A spec-complete MCP client should also be a *server* to
//! its servers: it advertises `sampling`, `roots`, and `elicitation`, and answers the matching
//! requests/notifications. [`ForgeClientHandler`] does that:
//!
//! - **`sampling/createMessage`** — a server asking the host to run an LLM turn. Routed to an
//!   optional [`SamplingHandler`] the host (forge-core) installs; absent → a clean
//!   method-not-found error (never a panic).
//! - **`roots/list`** — the workspace root(s) the host configured.
//! - **elicitation** — capability advertised; the minimal handler (rmcp's default) declines.
//! - **`notifications/tools/list_changed`** — re-list the server's tools and swap them into the
//!   shared live catalog, so runtime tool changes are picked up *without* a reconnect.
//!
//! The handler holds a [`Weak`] to the manager's connection map (the same map the manager reads),
//! so a `tools/list_changed` notification updates the authoritative catalog in place. `Weak` breaks
//! the ownership cycle (`Connection` owns the `RunningService` which owns this handler).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateMessageRequestMethod, CreateMessageRequestParams,
    CreateMessageResult, ListRootsResult, Root,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use rmcp::{ClientHandler, ErrorData as McpError};

use crate::Conns;

/// A boxed, sendable future returned by [`SamplingHandler::create_message`].
pub type SamplingFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CreateMessageResult, String>> + Send + 'a>>;

/// Host hook that fulfils a server-initiated `sampling/createMessage`. forge-core installs a real
/// implementation (routing the call through the mesh); when none is installed the handler returns
/// a method-not-found error rather than panicking. Kept as a trait so forge-mcp need not depend on
/// the provider/mesh crates — the boundary stays clean and the host owns the actual model call.
pub trait SamplingHandler: Send + Sync + 'static {
    fn create_message(&self, params: CreateMessageRequestParams) -> SamplingFuture<'_>;
}

/// The client handler instance for one server connection.
pub struct ForgeClientHandler {
    server_name: String,
    roots: Vec<Root>,
    sampling: Option<Arc<dyn SamplingHandler>>,
    /// Weak handle to the manager's connection map — upgraded on a `tools/list_changed` to refresh
    /// the live tool catalog in place. `Weak` so the handler never keeps the manager alive.
    conns: Weak<Conns>,
}

impl ForgeClientHandler {
    pub(crate) fn new(
        server_name: String,
        roots: Vec<Root>,
        sampling: Option<Arc<dyn SamplingHandler>>,
        conns: Weak<Conns>,
    ) -> Self {
        Self {
            server_name,
            roots,
            sampling,
            conns,
        }
    }

    /// A handler with no host hooks (no sampling, no roots, no catalog link) — used by the
    /// in-process test server and as a safe fallback. It still advertises the client capabilities.
    pub(crate) fn passive(server_name: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            roots: Vec::new(),
            sampling: None,
            conns: Weak::new(),
        }
    }
}

impl ClientHandler for ForgeClientHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        // Advertise the three client capabilities a spec-complete MCP client serves.
        info.capabilities = ClientCapabilities::builder()
            .enable_roots()
            .enable_sampling()
            .enable_elicitation()
            .build();
        info.client_info.name = "forge".to_string();
        info
    }

    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, McpError> {
        match &self.sampling {
            Some(handler) => handler
                .create_message(params)
                .await
                .map_err(|e| McpError::internal_error(format!("sampling failed: {e}"), None)),
            // No host sampling installed — decline cleanly instead of panicking.
            None => Err(McpError::method_not_found::<CreateMessageRequestMethod>()),
        }
    }

    async fn list_roots(
        &self,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, McpError> {
        Ok(ListRootsResult::new(self.roots.clone()))
    }

    async fn on_tool_list_changed(&self, ctx: NotificationContext<RoleClient>) {
        // Re-list the server's tools and swap them into the shared catalog so the change is live
        // without a reconnect. CRUCIAL: do the re-list (an outbound `tools/list` REQUEST) on a
        // SEPARATE task, not inline here. This handler runs on the client's service event loop; the
        // response to our `tools/list` is processed by that same loop, so awaiting it inline would
        // deadlock (loop waits for handler, handler waits for loop). Spawning returns immediately and
        // lets the loop process the response. The lock is taken only after the await, never held
        // across it.
        let Some(conns) = self.conns.upgrade() else {
            return;
        };
        let peer = ctx.peer.clone();
        let server = self.server_name.clone();
        tokio::spawn(async move {
            let tools = crate::discover_tools(&peer, &server).await;
            let mut map = conns.lock();
            if let Some(c) = map.get_mut(&server) {
                tracing::debug!(
                    "mcp: '{server}' sent tools/list_changed → refreshed to {} tool(s)",
                    tools.len()
                );
                c.tools = tools;
            }
        });
    }
}
