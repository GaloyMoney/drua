use serde::{Deserialize, Serialize};

use crate::primitives::*;

es_entity::entity_id! { ConversationId }

/// Status of a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationStatus {
    Active,
    Completed,
    Failed,
}

impl ConversationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConversationStatus::Active => "active",
            ConversationStatus::Completed => "completed",
            ConversationStatus::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ConversationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Role of a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Done,
    Error,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::ToolCall => "tool_call",
            MessageRole::ToolResult => "tool_result",
            MessageRole::Done => "done",
            MessageRole::Error => "error",
        }
    }
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A conversation between a user and an agent.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Conversation {
    pub id: ConversationId,
    pub agent_id: AgentId,
    pub user_id: UserId,
    pub status: String,
    pub total_tokens: i64,
    pub total_turns: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A single message within a conversation.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConversationMessage {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub sequence: i32,
    pub role: String,
    pub content: serde_json::Value,
    pub metadata: Option<serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
