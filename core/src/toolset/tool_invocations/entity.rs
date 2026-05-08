use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::primitives::{AgentId, ToolInvocationId};

/// One row from `tool_invocations`. Append-once after creation; all reads
/// hit the same row by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: ToolInvocationId,
    pub agent_id: AgentId,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub args_hash: Vec<u8>,
    pub classifier: String,
    pub summary: serde_json::Value,
    pub raw_text: String,
    pub raw_size_bytes: i64,
    pub exit_code: Option<i32>,
    pub duration_ms: i32,
    pub started_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Insert-only payload for [`ToolInvocations::persist`]. Plain struct rather
/// than a derive_builder shape because every field is required at dispatch
/// time and the call sites are internal to the dispatcher.
#[derive(Debug, Clone)]
pub struct NewToolInvocation {
    pub agent_id: AgentId,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub args_hash: Vec<u8>,
    pub classifier: String,
    pub summary: serde_json::Value,
    pub raw_text: String,
    pub raw_size_bytes: i64,
    pub exit_code: Option<i32>,
    pub duration_ms: i32,
    pub started_at: DateTime<Utc>,
}
