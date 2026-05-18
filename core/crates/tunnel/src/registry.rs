use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::protocol::{PendingResult, TunnelError, TunnelMessage, TUNNEL_TOOL_CALL_TIMEOUT};

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<PendingResult>>>>;

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

    pub async fn call_tool(
        &self,
        upstream: &str,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, TunnelError> {
        let id = uuid::Uuid::new_v4().to_string();
        let result = self
            .call_tool_inner(&id, upstream, tool_name, arguments)
            .await;
        self.pending.lock().await.remove(&id);
        result
    }

    async fn call_tool_inner(
        &self,
        id: &str,
        upstream: &str,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, TunnelError> {
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

        let json =
            serde_json::to_string(&msg).map_err(|e| TunnelError(format!("serialize: {e}")))?;

        if self.tx.send(json).await.is_err() {
            return Err(TunnelError("tunnel disconnected".to_string()));
        }

        tokio::time::timeout(TUNNEL_TOOL_CALL_TIMEOUT, resp_rx)
            .await
            .map_err(|_| {
                TunnelError(format!(
                    "tool call timed out after {}s",
                    TUNNEL_TOOL_CALL_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|_| TunnelError("tunnel disconnected".to_string()))?
            .map_err(TunnelError)
    }

    pub async fn resolve(&self, id: &str, result: PendingResult) {
        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.remove(id) {
            let _ = tx.send(result);
        }
    }

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

struct RegisteredTunnel {
    session_id: uuid::Uuid,
    close_tx: mpsc::Sender<()>,
    handle: TunnelHandle,
}

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

    pub async fn claim(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
        close_tx: mpsc::Sender<()>,
        handle: TunnelHandle,
    ) -> bool {
        let evicted = {
            let mut map = self.inner.lock().expect("tunnel registry lock poisoned");
            map.insert(
                deployment_id.to_string(),
                RegisteredTunnel {
                    session_id,
                    close_tx,
                    handle,
                },
            )
        };
        if let Some(old) = evicted {
            let _ = old.close_tx.try_send(());
            true
        } else {
            false
        }
    }

    /// Returns the session and handle for the connector WebSocket owned by this process.
    pub fn local_session(&self, deployment_id: &str) -> Option<(uuid::Uuid, TunnelHandle)> {
        let map = self.inner.lock().expect("tunnel registry lock poisoned");
        map.get(deployment_id)
            .map(|t| (t.session_id, t.handle.clone()))
    }

    pub async fn evict_if_session_differs(&self, deployment_id: &str, current: uuid::Uuid) -> bool {
        let close_tx = {
            let map = self.inner.lock().expect("tunnel registry lock poisoned");
            match map.get(deployment_id) {
                Some(t) if t.session_id != current => Some(t.close_tx.clone()),
                _ => None,
            }
        };
        close_tx
            .map(|tx| {
                let _ = tx.try_send(());
            })
            .is_some()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_starts_empty() {
        let registry = TunnelRegistry::new();
        assert!(registry.is_empty());
    }

    fn dummy_handle() -> TunnelHandle {
        let (tx, _rx) = mpsc::channel::<String>(1);
        TunnelHandle::new(tx)
    }

    #[tokio::test]
    async fn second_claim_evicts_first() {
        let registry = TunnelRegistry::new();

        let (tx_a, mut rx_a) = mpsc::channel::<()>(1);
        registry
            .claim("galoy-staging", uuid::Uuid::new_v4(), tx_a, dummy_handle())
            .await;

        let (tx_b, _rx_b) = mpsc::channel::<()>(1);
        let evicted = registry
            .claim("galoy-staging", uuid::Uuid::new_v4(), tx_b, dummy_handle())
            .await;

        assert!(evicted);
        assert_eq!(registry.len(), 1);
        assert!(rx_a.try_recv().is_ok());
    }

    #[tokio::test]
    async fn local_session_returns_session_and_handle() {
        let registry = TunnelRegistry::new();
        assert!(registry.local_session("galoy-staging").is_none());

        let session = uuid::Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<()>(1);
        registry
            .claim("galoy-staging", session, tx, dummy_handle())
            .await;

        let (sid, _handle) = registry.local_session("galoy-staging").expect("present");
        assert_eq!(sid, session);
    }

    #[tokio::test]
    async fn evict_if_session_differs_signals_close_when_mismatched() {
        let registry = TunnelRegistry::new();
        let local_session = uuid::Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<()>(1);
        registry
            .claim("galoy-staging", local_session, tx, dummy_handle())
            .await;

        let evicted = registry
            .evict_if_session_differs("galoy-staging", local_session)
            .await;
        assert!(!evicted);
        assert!(rx.try_recv().is_err());

        let other_session = uuid::Uuid::new_v4();
        let evicted = registry
            .evict_if_session_differs("galoy-staging", other_session)
            .await;
        assert!(evicted);
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn release_with_stale_session_id_is_noop() {
        let registry = TunnelRegistry::new();

        let stale_session = uuid::Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel::<()>(1);
        registry
            .claim("galoy-staging", stale_session, tx_a, dummy_handle())
            .await;

        let fresh_session = uuid::Uuid::new_v4();
        let (tx_b, _rx_b) = mpsc::channel::<()>(1);
        registry
            .claim("galoy-staging", fresh_session, tx_b, dummy_handle())
            .await;

        registry.release("galoy-staging", stale_session);
        assert_eq!(registry.len(), 1);

        registry.release("galoy-staging", fresh_session);
        assert!(registry.is_empty());
    }

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
        assert!(matches!(result, Err(TunnelError(_))));
        assert_eq!(handle.pending_len().await, 0);
    }
}
