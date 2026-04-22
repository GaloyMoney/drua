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
    ///
    /// Every non-happy-path exit (timeout, tunnel closed mid-call, send
    /// failure) removes the id from `pending` before returning — otherwise
    /// the entry (and the `oneshot::Sender` it holds) would linger for the
    /// lifetime of every cloned [`TunnelHandle`], which is the entire
    /// registered `TunnelToolSet`. That's the leak the PR #127 review
    /// flagged.
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
        // Single cleanup point: remove is a no-op if a successful `resolve`
        // already took the entry out. Covers timeout, send failure, and
        // serialization failure without repeating cleanup at each `?`.
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

    /// Resolve a pending call with a result (called by the WebSocket handler).
    pub async fn resolve(&self, id: &str, result: Result<CallToolResult, String>) {
        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.remove(id) {
            let _ = tx.send(result);
        }
    }

    /// Drain every outstanding pending call with `error`. Called from the
    /// WS handler's cleanup block when the relay loop exits, so in-flight
    /// callers fail immediately instead of waiting the full 120s timeout
    /// for a response that will never arrive.
    ///
    /// Cleanup ordering in the WS handler (see `web/src/tunnel.rs`):
    ///   1. `unregister_searchable_by_session` — no new calls can reach us.
    ///   2. `fail_all_pending` — drain anything that beat the unregister.
    ///   3. `TunnelRegistry::release`.
    pub async fn fail_all_pending(&self, error: &str) {
        let mut pending = self.pending.lock().await;
        let drained: Vec<_> = pending.drain().collect();
        drop(pending);
        for (_, tx) in drained {
            let _ = tx.send(Err(error.to_string()));
        }
    }

    /// Number of outstanding pending tool calls. Test/observability helper.
    #[cfg(test)]
    pub async fn pending_len(&self) -> usize {
        self.pending.lock().await.len()
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
    /// Scope tag — both the `deployment_id` (so
    /// [`super::toolset::ToolSets::replace_tunnel_toolsets`] can atomically
    /// swap a deployment's toolsets on takeover) and the `session_id`
    /// (so an evicted WS loop's cleanup doesn't remove the live session's
    /// entries).
    scope: ToolSetScope,
}

impl TunnelToolSet {
    /// Build a toolset from a connector's registration entry.
    ///
    /// The `name` is scoped to the deployment (e.g. `staging-kubernetes`)
    /// and the `prefix` is scoped similarly (e.g. `staging_k8s`) so that
    /// multiple deployments' toolsets don't collide in the catalog.
    /// `session_id` identifies the WS session that owns this toolset —
    /// cleanup keys on it so takeover is safe.
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

    // ── TunnelHandle pending-map tests ──────────────────────────────────
    //
    // Each scenario below proves that `pending` does not leak under an
    // abnormal exit path. Before these fixes, an in-flight call that hit
    // a disconnect-after-send or a timeout would leave its `oneshot::Sender`
    // in the map forever, pinned alive by cloned `TunnelHandle`s inside
    // the registered `TunnelToolSet`s.

    /// Successful resolve: happy path — entry is removed by `resolve`,
    /// and the outer cleanup in `call_tool` is a no-op.
    #[tokio::test]
    async fn pending_cleared_on_success() {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);

        // Consume the outbound message so send doesn't block the channel.
        // Resolve after a brief delay so the caller is already awaiting.
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

    /// Send failure: outbound receiver dropped before the call goes out.
    /// The entry must still be cleaned up — otherwise every call after a
    /// disconnect would leak.
    #[tokio::test]
    async fn pending_cleared_on_send_failure() {
        let (tx, rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);
        drop(rx); // simulate relay loop already gone

        let result = handle.call_tool("kubernetes", "list_pods", None).await;
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
        assert_eq!(handle.pending_len().await, 0);
    }

    /// `fail_all_pending` drains outstanding calls and fails them
    /// immediately, instead of them sitting on the full 120s timeout.
    /// This is the fix for PR #127's "callers sit 120s after tunnel
    /// death" review comment.
    #[tokio::test(start_paused = true)]
    async fn fail_all_pending_drains_immediately() {
        let (tx, _rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);

        // Launch a call that will park on resp_rx forever without intervention.
        let caller = {
            let h = handle.clone();
            tokio::spawn(async move { h.call_tool("k8s", "get_pods", None).await })
        };

        // Let the caller reach the await point and register in pending.
        tokio::task::yield_now().await;
        // Spin until the pending entry is visible — avoids a timing race
        // on slow runners without real-wall-clock sleep (tokio is paused).
        for _ in 0..100 {
            if handle.pending_len().await == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(handle.pending_len().await, 1);

        handle.fail_all_pending("tunnel disconnected").await;

        // Caller should return essentially immediately, not after 120s.
        let result = caller.await.unwrap();
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
        assert_eq!(handle.pending_len().await, 0);
    }

    /// Timeout path: a call whose resp_rx never fires must clean its own
    /// pending entry. Using `start_paused = true` so the 120s timeout
    /// fires in virtual time without blocking the test for 2 minutes.
    #[tokio::test(start_paused = true)]
    async fn pending_cleared_on_timeout() {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let handle = TunnelHandle::new(tx);

        let caller = {
            let h = handle.clone();
            tokio::spawn(async move { h.call_tool("k8s", "get_pods", None).await })
        };

        // Drain the outbound queue so send succeeds — otherwise the call
        // would exit via the send-failure path instead of timeout.
        let _outbound = rx.recv().await.expect("outbound message");

        // Advance past the 120s timeout.
        tokio::time::advance(std::time::Duration::from_secs(121)).await;

        let result = caller.await.unwrap();
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
        assert_eq!(handle.pending_len().await, 0);
    }
}
