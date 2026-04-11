use serde::{Deserialize, Serialize};

use crate::primitives::*;

es_entity::entity_id! { SessionId }

/// Status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Completed,
    Error,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Completed => "completed",
            SessionStatus::Error => "error",
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Type of a session event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventType {
    System,
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Error,
    Compaction,
}

impl SessionEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionEventType::System => "system",
            SessionEventType::User => "user",
            SessionEventType::Assistant => "assistant",
            SessionEventType::ToolCall => "tool_call",
            SessionEventType::ToolResult => "tool_result",
            SessionEventType::Error => "error",
            SessionEventType::Compaction => "compaction",
        }
    }
}

impl std::fmt::Display for SessionEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A session record (one per agent interaction).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub agent_id: AgentId,
    pub workspace_id: WorkspaceId,
    pub model: Option<String>,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_turns: Option<i32>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A single event within a session.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct SessionEventRow {
    pub id: i64,
    pub session_id: SessionId,
    pub seq: i32,
    pub event_type: String,
    pub role: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub content: serde_json::Value,
    pub metadata: Option<serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A new session event to be inserted.
#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub event_type: SessionEventType,
    pub role: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub content: serde_json::Value,
    pub metadata: serde_json::Value,
}

/// Stats recorded when a session completes.
#[derive(Debug, Clone)]
pub struct SessionStats {
    pub total_turns: u32,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}
