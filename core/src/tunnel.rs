//! Tunnel transport for deployment-side MCP servers.
//!
//! Instead of exposing MCP servers on a public endpoint with JWT auth,
//! a lightweight connector in the target cluster dials *out* to galoy-agents
//! over WebSocket. Tool calls are relayed through the already-authenticated
//! tunnel — no ingress, Envoy, or JWT validation required in the target
//! cluster.
//!
//! # Wire protocol
//!
//! All messages are JSON text frames:
//!
//! - **Register** (connector → server): sent once after connect; carries
//!   deployment identity and the full tool catalog discovered from local
//!   MCP servers.
//! - **CallTool** (server → connector): a tool invocation request with a
//!   correlation `id`.
//! - **CallToolResult / CallToolError** (connector → server): the response,
//!   matched by `id`.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::toolset::{SearchableToolSet, ToolSetEntry, ToolSetsError};

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelMessage {
    /// Connector → Server: register deployment and discovered tools.
    Register {
        deployment_id: String,
        toolsets: Vec<RegisteredToolSet>,
    },
    /// Server → Connector: invoke a tool on a local MCP server.
    CallTool {
        id: String,
        upstream: String,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// Connector → Server: successful tool result.
    CallToolResult {
        id: String,
        result: serde_json::Value,
    },
    /// Connector → Server: tool call failed.
    CallToolError { id: String, error: String },
}

/// A toolset advertised by the connector during registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredToolSet {
    pub name: String,
    pub prefix: String,
    pub category: String,
    pub category_description: String,
    /// Each entry is a JSON-serialized `rmcp::model::Tool`.
    pub tools: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Tunnel handle — one per WebSocket connection
// ---------------------------------------------------------------------------

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<CallToolResult, String>>>>>;

/// Shared handle for sending tool calls through a tunnel and awaiting results.
#[derive(Clone)]
pub struct TunnelHandle {
    tx: mpsc::Sender<String>,
    pending: PendingMap,
}

impl TunnelHandle {
    pub fn new(tx: mpsc::Sender<String>) -> Self {
        Self {
            tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Send a tool call through the tunnel and wait for the result.
    pub async fn call_tool(
        &self,
        upstream: &str,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let id = uuid::Uuid::new_v4().to_string();
        let (resp_tx, resp_rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.clone(), resp_tx);
        }

        let msg = TunnelMessage::CallTool {
            id: id.clone(),
            upstream: upstream.to_string(),
            tool_name: tool_name.to_string(),
            arguments,
        };

        let json = serde_json::to_string(&msg)
            .map_err(|e| ToolSetsError::Tunnel(format!("serialize: {e}")))?;

        if self.tx.send(json).await.is_err() {
            let mut pending = self.pending.lock().await;
            pending.remove(&id);
            return Err(ToolSetsError::Tunnel("tunnel disconnected".to_string()));
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(120), resp_rx)
            .await
            .map_err(|_| ToolSetsError::Tunnel("tool call timed out after 120s".to_string()))?
            .map_err(|_| ToolSetsError::Tunnel("tunnel disconnected".to_string()))?
            .map_err(ToolSetsError::Tunnel)?;

        Ok(result)
    }

    /// Resolve a pending call with a result (called by the WebSocket handler).
    pub async fn resolve(&self, id: &str, result: Result<CallToolResult, String>) {
        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.remove(id) {
            let _ = tx.send(result);
        }
    }
}

// ---------------------------------------------------------------------------
// TunnelRegistry — enforces one live tunnel per deployment_id
// ---------------------------------------------------------------------------

/// Per-live-tunnel entry. The `close_tx` is how the registry signals the
/// old WS loop to shut down when a new connector takes over the same
/// `deployment_id`. The `session_id` lets an evicted loop avoid
/// accidentally removing the *new* entry during its own cleanup.
struct RegisteredTunnel {
    session_id: uuid::Uuid,
    close_tx: mpsc::Sender<()>,
}

/// Maps `deployment_id` → the currently live tunnel session for that
/// deployment. At most one entry per `deployment_id` at any time — a
/// new Register for an already-registered id evicts the previous
/// connection (closes its WS with a `POLICY` close frame).
///
/// This closes gap #2 from drua#127's original "Known security gaps"
/// list: without the registry, two connectors holding the same
/// `deployment_id` would race-fight for tool call results via the
/// shared `TunnelHandle::resolve` pending map. With the registry,
/// the second connector's registration cleanly replaces the first.
#[derive(Clone)]
pub struct TunnelRegistry {
    inner: Arc<std::sync::Mutex<HashMap<String, RegisteredTunnel>>>,
}

impl TunnelRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Claim `deployment_id` for a new session. If an entry already
    /// exists, its `close_tx` is taken out, the new one inserted under
    /// the lock, and the old `close_tx` signaled *after* the lock is
    /// released (await outside the critical section). Returns `true` if
    /// an existing tunnel was evicted.
    pub async fn claim(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
        close_tx: mpsc::Sender<()>,
    ) -> bool {
        let evicted = {
            let mut map = self.inner.lock().expect("tunnel registry lock poisoned");
            map.insert(
                deployment_id.to_string(),
                RegisteredTunnel {
                    session_id,
                    close_tx,
                },
            )
        };
        if let Some(old) = evicted {
            // Channel capacity is 1; ignore send errors — if the receiver
            // already dropped, the old loop is already on its way out.
            let _ = old.close_tx.send(()).await;
            true
        } else {
            false
        }
    }

    /// Remove the entry for `deployment_id` only if it still belongs to
    /// `session_id`. Called during cleanup so an evicted loop doesn't
    /// accidentally remove the *new* tunnel's registry entry.
    pub fn release(&self, deployment_id: &str, session_id: uuid::Uuid) {
        let mut map = self.inner.lock().expect("tunnel registry lock poisoned");
        if let Some(entry) = map.get(deployment_id) {
            if entry.session_id == session_id {
                map.remove(deployment_id);
            }
        }
    }

    /// Number of currently-registered deployments. Test/observability helper.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("tunnel registry lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for TunnelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// TunnelToolSet — SearchableToolSet backed by a tunnel connection
// ---------------------------------------------------------------------------

pub struct TunnelToolSet {
    name: String,
    prefix: String,
    category: String,
    category_description: String,
    upstream_name: String,
    tools: Vec<ToolSetEntry>,
    handle: TunnelHandle,
}

impl TunnelToolSet {
    /// Build a toolset from a connector's registration entry.
    ///
    /// The `name` is scoped to the deployment (e.g. `staging-kubernetes`)
    /// and the `prefix` is scoped similarly (e.g. `staging_k8s`) so that
    /// multiple deployments' toolsets don't collide in the catalog.
    pub fn new(
        deployment_id: &str,
        registration: &RegisteredToolSet,
        handle: TunnelHandle,
    ) -> Result<Self, String> {
        let tools: Vec<ToolSetEntry> = registration
            .tools
            .iter()
            .filter_map(|t| {
                let tool: Tool = serde_json::from_value(t.clone()).ok()?;
                Some(ToolSetEntry {
                    name: tool.name.to_string(),
                    description: tool,
                    default_output_filter: None,
                })
            })
            .collect();

        let name = format!("{}-{}", deployment_id, registration.name);
        let prefix = format!("{}_{}", deployment_id, registration.prefix);

        Ok(Self {
            name,
            prefix,
            category: registration.category.clone(),
            category_description: registration.category_description.clone(),
            upstream_name: registration.name.clone(),
            tools,
            handle,
        })
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for TunnelToolSet {
    fn name(&self) -> &str {
        &self.name
    }

    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn category(&self) -> &str {
        &self.category
    }

    fn category_description(&self) -> &str {
        &self.category_description
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        self.handle
            .call_tool(&self.upstream_name, tool_name, arguments)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh registry starts empty.
    #[tokio::test]
    async fn registry_starts_empty() {
        let registry = TunnelRegistry::new();
        assert!(registry.is_empty());
    }

    /// First claim for a deployment_id succeeds without eviction.
    #[tokio::test]
    async fn claim_first_does_not_evict() {
        let registry = TunnelRegistry::new();
        let (tx, _rx) = mpsc::channel::<()>(1);
        let evicted = registry
            .claim("galoy-staging", uuid::Uuid::new_v4(), tx)
            .await;
        assert!(!evicted);
        assert_eq!(registry.len(), 1);
    }

    /// A second claim for the same deployment_id evicts the first.
    /// The first session's `close_rx` receives the eviction signal.
    #[tokio::test]
    async fn second_claim_evicts_first() {
        let registry = TunnelRegistry::new();

        let (tx_a, mut rx_a) = mpsc::channel::<()>(1);
        registry
            .claim("galoy-staging", uuid::Uuid::new_v4(), tx_a)
            .await;

        let (tx_b, _rx_b) = mpsc::channel::<()>(1);
        let evicted = registry
            .claim("galoy-staging", uuid::Uuid::new_v4(), tx_b)
            .await;

        assert!(evicted);
        // Still one entry — the new one replaced the old.
        assert_eq!(registry.len(), 1);
        // The evicted session received the close signal.
        assert!(rx_a.try_recv().is_ok());
    }

    /// Different deployment_ids coexist without eviction.
    #[tokio::test]
    async fn distinct_deployments_coexist() {
        let registry = TunnelRegistry::new();
        let (tx_a, _rx_a) = mpsc::channel::<()>(1);
        let (tx_b, _rx_b) = mpsc::channel::<()>(1);

        assert!(
            !registry
                .claim("galoy-staging", uuid::Uuid::new_v4(), tx_a)
                .await
        );
        assert!(
            !registry
                .claim("galoy-production", uuid::Uuid::new_v4(), tx_b)
                .await
        );
        assert_eq!(registry.len(), 2);
    }

    /// `release` with the current session_id removes the entry.
    #[tokio::test]
    async fn release_removes_current_entry() {
        let registry = TunnelRegistry::new();
        let session = uuid::Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<()>(1);
        registry.claim("galoy-staging", session, tx).await;

        registry.release("galoy-staging", session);
        assert!(registry.is_empty());
    }

    /// `release` with a *stale* session_id (e.g. an evicted loop cleaning
    /// up after the new loop has taken over) must not remove the new
    /// entry. This is the invariant that keeps eviction safe.
    #[tokio::test]
    async fn release_with_stale_session_id_is_noop() {
        let registry = TunnelRegistry::new();

        let stale_session = uuid::Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel::<()>(1);
        registry.claim("galoy-staging", stale_session, tx_a).await;

        let fresh_session = uuid::Uuid::new_v4();
        let (tx_b, _rx_b) = mpsc::channel::<()>(1);
        registry.claim("galoy-staging", fresh_session, tx_b).await;

        // Evicted loop calls release with its own (now stale) session id.
        // Should not affect the fresh entry.
        registry.release("galoy-staging", stale_session);
        assert_eq!(registry.len(), 1);

        // Fresh session can still release itself.
        registry.release("galoy-staging", fresh_session);
        assert!(registry.is_empty());
    }
}
