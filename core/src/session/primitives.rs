use serde::{Deserialize, Serialize};

use crate::primitives::*;

es_entity::entity_id! { SessionId }

/// Runtime that executed the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntime {
    Sandbox,
    Light,
}

impl SessionRuntime {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionRuntime::Sandbox => "sandbox",
            SessionRuntime::Light => "light",
        }
    }
}

impl std::fmt::Display for SessionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Completed,
    Failed,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Completed => "completed",
            SessionStatus::Failed => "failed",
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalized event type for session events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventType {
    SessionInit,
    UserMessage,
    AssistantText,
    ToolCall,
    ToolResult,
    Thinking,
    SessionEnd,
    Error,
    Status,
    ApiRetry,
}

impl SessionEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionEventType::SessionInit => "session_init",
            SessionEventType::UserMessage => "user_message",
            SessionEventType::AssistantText => "assistant_text",
            SessionEventType::ToolCall => "tool_call",
            SessionEventType::ToolResult => "tool_result",
            SessionEventType::Thinking => "thinking",
            SessionEventType::SessionEnd => "session_end",
            SessionEventType::Error => "error",
            SessionEventType::Status => "status",
            SessionEventType::ApiRetry => "api_retry",
        }
    }

    /// Map to a backward-compatible role string for the chat UI.
    pub fn role(&self) -> Option<&'static str> {
        match self {
            SessionEventType::UserMessage => Some("user"),
            SessionEventType::AssistantText
            | SessionEventType::ToolCall
            | SessionEventType::Thinking => Some("assistant"),
            SessionEventType::ToolResult => Some("user"),
            SessionEventType::SessionInit
            | SessionEventType::SessionEnd
            | SessionEventType::Error
            | SessionEventType::Status
            | SessionEventType::ApiRetry => Some("system"),
        }
    }
}

impl std::fmt::Display for SessionEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A new session event to be recorded.
#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub event_type: SessionEventType,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub content: serde_json::Value,
    pub metadata: serde_json::Value,
    pub raw_event: Option<serde_json::Value>,
}

/// Final statistics recorded when a session ends.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub claude_session_id: Option<String>,
    pub total_turns: i32,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
}

/// A persisted session row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub agent_id: AgentId,
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub claude_session_id: Option<String>,
    pub runtime: String,
    pub model: Option<String>,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_turns: Option<i32>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A persisted session event row.
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
    pub raw_event: Option<serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
