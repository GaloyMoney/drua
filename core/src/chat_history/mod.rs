pub mod error;
pub mod primitives;

use tracing::instrument;

pub use error::*;
pub use primitives::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct ChatHistory {
    pool: sqlx::PgPool,
}

impl ChatHistory {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Create a new conversation for the given agent and user.
    #[instrument(name = "chat_history.create_conversation", skip_all)]
    pub async fn create_conversation(
        &self,
        agent_id: AgentId,
        user_id: UserId,
    ) -> Result<Conversation, ChatHistoryError> {
        let id = ConversationId::new();

        let row = sqlx::query_as::<_, Conversation>(
            r#"INSERT INTO conversations (id, agent_id, user_id)
            VALUES ($1, $2, $3)
            RETURNING id, agent_id, user_id, status, total_tokens, total_turns, created_at, updated_at"#,
        )
        .bind(id)
        .bind(agent_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Append a message to a conversation, auto-incrementing the sequence number.
    #[instrument(name = "chat_history.record_message", skip_all)]
    pub async fn record_message(
        &self,
        conversation_id: ConversationId,
        role: MessageRole,
        content: serde_json::Value,
        metadata: serde_json::Value,
    ) -> Result<ConversationMessage, ChatHistoryError> {
        let role_str = role.as_str();

        let row = sqlx::query_as::<_, ConversationMessage>(
            r#"INSERT INTO conversation_messages (conversation_id, sequence, role, content, metadata)
            VALUES (
                $1,
                COALESCE((SELECT MAX(sequence) FROM conversation_messages WHERE conversation_id = $1), -1) + 1,
                $2,
                $3,
                $4
            )
            RETURNING id, conversation_id, sequence, role, content, metadata, created_at"#,
        )
        .bind(conversation_id)
        .bind(role_str)
        .bind(&content)
        .bind(&metadata)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Store the full Anthropic API messages array on the conversation (for light agent resume).
    #[instrument(name = "chat_history.update_api_messages", skip_all)]
    pub async fn update_api_messages(
        &self,
        conversation_id: ConversationId,
        messages: &serde_json::Value,
    ) -> Result<(), ChatHistoryError> {
        let result = sqlx::query(
            r#"UPDATE conversations
            SET api_messages = $1, updated_at = NOW()
            WHERE id = $2"#,
        )
        .bind(messages)
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(ChatHistoryError::ConversationNotFound);
        }
        Ok(())
    }

    /// Mark a conversation as completed with final stats.
    #[instrument(name = "chat_history.complete_conversation", skip_all)]
    pub async fn complete_conversation(
        &self,
        conversation_id: ConversationId,
        status: ConversationStatus,
        total_turns: i32,
        total_tokens: i64,
    ) -> Result<(), ChatHistoryError> {
        let status_str = status.as_str();

        let result = sqlx::query(
            r#"UPDATE conversations
            SET status = $1, total_turns = $2, total_tokens = $3, updated_at = NOW()
            WHERE id = $4"#,
        )
        .bind(status_str)
        .bind(total_turns)
        .bind(total_tokens)
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(ChatHistoryError::ConversationNotFound);
        }
        Ok(())
    }

    /// Find a conversation by its ID.
    #[instrument(name = "chat_history.find_conversation", skip_all)]
    pub async fn find_conversation(
        &self,
        id: ConversationId,
    ) -> Result<Conversation, ChatHistoryError> {
        sqlx::query_as::<_, Conversation>(
            r#"SELECT id, agent_id, user_id, status, total_tokens, total_turns, created_at, updated_at
            FROM conversations
            WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ChatHistoryError::ConversationNotFound)
    }

    /// List conversations for an agent (most recent first).
    #[instrument(name = "chat_history.list_by_agent", skip_all)]
    pub async fn list_by_agent(
        &self,
        agent_id: AgentId,
        limit: i64,
    ) -> Result<Vec<Conversation>, ChatHistoryError> {
        let rows = sqlx::query_as::<_, Conversation>(
            r#"SELECT id, agent_id, user_id, status, total_tokens, total_turns, created_at, updated_at
            FROM conversations
            WHERE agent_id = $1
            ORDER BY created_at DESC
            LIMIT $2"#,
        )
        .bind(agent_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Get all messages for a conversation, ordered by sequence.
    #[instrument(name = "chat_history.get_messages", skip_all)]
    pub async fn get_messages(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<ConversationMessage>, ChatHistoryError> {
        let rows = sqlx::query_as::<_, ConversationMessage>(
            r#"SELECT id, conversation_id, sequence, role, content, metadata, created_at
            FROM conversation_messages
            WHERE conversation_id = $1
            ORDER BY sequence ASC"#,
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
