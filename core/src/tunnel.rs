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
