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

use crate::auth::AuthSubject;
use crate::toolset::{SearchableToolSet, ToolSetEntry, ToolSetScope, ToolSetsError};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredToolSet {
    pub name: String,
    pub prefix: String,
    pub category: String,
    pub category_description: String,
    /// Each entry is a JSON-serialized `rmcp::model::Tool`.
    pub tools: Vec<serde_json::Value>,
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<CallToolResult, String>>>>>;

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

    /// Every exit path removes the id from `pending` to avoid leaking the
    /// `oneshot::Sender` for the lifetime of the cloned handle (PR #127).
    pub async fn call_tool(
        &self,
        upstream: &str,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let id = uuid::Uuid::new_v4().to_string();
        let result = self
            .call_tool_inner(&id, upstream, tool_name, arguments)
            .await;
        // Single cleanup; `resolve` may have taken the entry already.
        self.pending.lock().await.remove(&id);
        result
    }

    async fn call_tool_inner(
        &self,
        id: &str,
        upstream: &str,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let (resp_tx, resp_rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.to_string(), resp_tx);
        }

        let msg = TunnelMessage::CallTool {
            id: id.to_string(),
            upstream: upstream.to_string(),
            tool_name: tool_name.to_string(),
            arguments,
        };

        let json = serde_json::to_string(&msg)
            .map_err(|e| ToolSetsError::Tunnel(format!("serialize: {e}")))?;

        if self.tx.send(json).await.is_err() {
            return Err(ToolSetsError::Tunnel("tunnel disconnected".to_string()));
        }

        tokio::time::timeout(std::time::Duration::from_secs(120), resp_rx)
            .await
            .map_err(|_| ToolSetsError::Tunnel("tool call timed out after 120s".to_string()))?
            .map_err(|_| ToolSetsError::Tunnel("tunnel disconnected".to_string()))?
            .map_err(ToolSetsError::Tunnel)
    }

    pub async fn resolve(&self, id: &str, result: Result<CallToolResult, String>) {
        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.remove(id) {
            let _ = tx.send(result);
        }
    }

    /// Cleanup order (`web/src/tunnel.rs`):
    /// 1. `unregister_searchable_by_session` (no new calls)
    /// 2. `fail_all_pending` (drain in-flight)
    /// 3. `TunnelRegistry::release`
    pub async fn fail_all_pending(&self, error: &str) {
        let mut pending = self.pending.lock().await;
        let drained: Vec<_> = pending.drain().collect();
        drop(pending);
        for (_, tx) in drained {
            let _ = tx.send(Err(error.to_string()));
        }
    }

    #[cfg(test)]
    pub async fn pending_len(&self) -> usize {
        self.pending.lock().await.len()
    }
}

/// `close_tx` evicts old WS loop on takeover; `session_id` prevents an
/// evicted loop from removing the new entry during cleanup.
struct RegisteredTunnel {
    session_id: uuid::Uuid,
    close_tx: mpsc::Sender<()>,
}

/// At most one entry per `deployment_id`; a new Register evicts the
/// previous connection (POLICY close frame). Closes drua#127 gap #2:
/// prevents two connectors fighting over the shared `pending` map.
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

    /// Old `close_tx` is signaled *outside* the lock. Returns `true` if evicted.
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
            // Capacity 1; receiver-dropped means old loop already exiting.
            let _ = old.close_tx.send(()).await;
            true
        } else {
            false
        }
    }

    /// Removes only if still owned by `session_id` — evicted loops don't
    /// trample the new entry.
    pub fn release(&self, deployment_id: &str, session_id: uuid::Uuid) {
        let mut map = self.inner.lock().expect("tunnel registry lock poisoned");
        if let Some(entry) = map.get(deployment_id) {
            if entry.session_id == session_id {
                map.remove(deployment_id);
            }
        }
    }

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

pub struct TunnelToolSet {
    name: String,
    prefix: String,
    category: String,
    category_description: String,
    upstream_name: String,
    tools: Vec<ToolSetEntry>,
    handle: TunnelHandle,
    /// `deployment_id` enables atomic takeover swap; `session_id` keeps
    /// evicted-loop cleanup from removing the live session's entries.
    scope: ToolSetScope,
}

impl TunnelToolSet {
    /// Name/prefix are scoped to `deployment_id` to avoid catalog collisions.
    pub fn new(
        deployment_id: &str,
        session_id: uuid::Uuid,
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
                })
            })
            .collect();

        let name = format!("{}_{}", deployment_id, registration.name).replace('-', "_");
        let prefix = format!("{}_{}", deployment_id, registration.prefix).replace('-', "_");

        Ok(Self {
            name,
            prefix,
            category: registration.category.clone(),
            category_description: registration.category_description.clone(),
            upstream_name: registration.name.clone(),
            tools,
            handle,
            scope: ToolSetScope::Tunnel {
                deployment_id: deployment_id.to_string(),
                session_id,
            },
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

    fn scope(&self) -> Option<&ToolSetScope> {
        Some(&self.scope)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
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

    #[tokio::test]
    async fn registry_starts_empty() {
        let registry = TunnelRegistry::new();
        assert!(registry.is_empty());
    }

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
        assert_eq!(registry.len(), 1);
        assert!(rx_a.try_recv().is_ok());
    }

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

    #[tokio::test]
    async fn release_removes_current_entry() {
        let registry = TunnelRegistry::new();
        let session = uuid::Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<()>(1);
        registry.claim("galoy-staging", session, tx).await;

        registry.release("galoy-staging", session);
        assert!(registry.is_empty());
    }

    /// Stale session_id must not remove the live entry.
    #[tokio::test]
    async fn release_with_stale_session_id_is_noop() {
        let registry = TunnelRegistry::new();

        let stale_session = uuid::Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel::<()>(1);
        registry.claim("galoy-staging", stale_session, tx_a).await;

        let fresh_session = uuid::Uuid::new_v4();
        let (tx_b, _rx_b) = mpsc::channel::<()>(1);
        registry.claim("galoy-staging", fresh_session, tx_b).await;

        registry.release("galoy-staging", stale_session);
        assert_eq!(registry.len(), 1);

        registry.release("galoy-staging", fresh_session);
        assert!(registry.is_empty());
    }

    // Each scenario below proves `pending` doesn't leak under abnormal exit.

    #[tokio::test]
    async fn pending_cleared_on_success() {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);

        let handle_bg = handle.clone();
        let resolver = tokio::spawn(async move {
            let json = rx.recv().await.expect("outbound");
            let parsed: TunnelMessage = serde_json::from_str(&json).unwrap();
            let id = match parsed {
                TunnelMessage::CallTool { id, .. } => id,
                _ => panic!("expected CallTool"),
            };
            let result: CallToolResult =
                serde_json::from_value(serde_json::json!({ "content": [], "isError": false }))
                    .unwrap();
            handle_bg.resolve(&id, Ok(result)).await;
        });

        let result = handle.call_tool("kubernetes", "list_pods", None).await;
        assert!(result.is_ok());
        resolver.await.unwrap();
        assert_eq!(handle.pending_len().await, 0);
    }

    #[tokio::test]
    async fn pending_cleared_on_send_failure() {
        let (tx, rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);
        drop(rx);

        let result = handle.call_tool("kubernetes", "list_pods", None).await;
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
        assert_eq!(handle.pending_len().await, 0);
    }

    /// PR #127 fix: avoid 120s timeout after tunnel death.
    #[tokio::test(start_paused = true)]
    async fn fail_all_pending_drains_immediately() {
        let (tx, _rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);

        let caller = {
            let h = handle.clone();
            tokio::spawn(async move { h.call_tool("k8s", "get_pods", None).await })
        };

        tokio::task::yield_now().await;
        // Spin to avoid timing race; tokio is paused so no wall-clock sleep.
        for _ in 0..100 {
            if handle.pending_len().await == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(handle.pending_len().await, 1);

        handle.fail_all_pending("tunnel disconnected").await;

        let result = caller.await.unwrap();
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
        assert_eq!(handle.pending_len().await, 0);
    }

    /// `start_paused = true` advances the 120s timeout in virtual time.
    #[tokio::test(start_paused = true)]
    async fn pending_cleared_on_timeout() {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);

        let caller = {
            let h = handle.clone();
            tokio::spawn(async move { h.call_tool("k8s", "get_pods", None).await })
        };

        // Drain so send succeeds (else exits via send-failure not timeout).
        let _outbound = rx.recv().await.expect("outbound message");

        tokio::time::advance(std::time::Duration::from_secs(121)).await;

        let result = caller.await.unwrap();
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
        assert_eq!(handle.pending_len().await, 0);
    }
}
