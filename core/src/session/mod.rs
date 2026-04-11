pub mod error;
pub mod primitives;

use tracing::instrument;

pub use error::*;
pub use primitives::*;

use crate::primitives::*;

/// Flat SQL service for rich session event persistence.
///
/// Sessions capture the full interaction history for light agents, including
/// system prompts, user messages, assistant responses, tool calls with their
/// inputs, and tool results with output and correlation IDs.
#[derive(Clone)]
pub struct Sessions {
    pool: sqlx::PgPool,
}

impl Sessions {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Create a new session for the given agent.
    #[instrument(name = "session.create", skip_all)]
    pub async fn create_session(
        &self,
        agent_id: AgentId,
        workspace_id: WorkspaceId,
    ) -> Result<Session, SessionError> {
        let id = SessionId::new();

        let row = sqlx::query_as::<_, Session>(
            r#"INSERT INTO sessions (id, agent_id, workspace_id)
            VALUES ($1, $2, $3)
            RETURNING id, agent_id, workspace_id, status,
                      started_at, ended_at, total_turns,
                      total_input_tokens, total_output_tokens,
                      created_at, updated_at"#,
        )
        .bind(id)
        .bind(agent_id)
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Append a single event to a session, auto-incrementing the sequence number.
    #[instrument(name = "session.record_event", skip_all)]
    pub async fn record_event(
        &self,
        session_id: SessionId,
        event: NewSessionEvent,
    ) -> Result<(), SessionError> {
        sqlx::query(
            r#"INSERT INTO session_events (session_id, seq, event_type, role, tool_name, tool_use_id, content, metadata)
            VALUES (
                $1,
                COALESCE((SELECT MAX(seq) FROM session_events WHERE session_id = $1), -1) + 1,
                $2, $3, $4, $5, $6, $7
            )"#,
        )
        .bind(session_id)
        .bind(event.event_type.as_str())
        .bind(&event.role)
        .bind(&event.tool_name)
        .bind(&event.tool_use_id)
        .bind(&event.content)
        .bind(&event.metadata)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append multiple events in a single transaction (preserves ordering).
    #[instrument(name = "session.record_events", skip_all)]
    pub async fn record_events(
        &self,
        session_id: SessionId,
        events: Vec<NewSessionEvent>,
    ) -> Result<(), SessionError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;

        for event in events {
            sqlx::query(
                r#"INSERT INTO session_events (session_id, seq, event_type, role, tool_name, tool_use_id, content, metadata)
                VALUES (
                    $1,
                    COALESCE((SELECT MAX(seq) FROM session_events WHERE session_id = $1), -1) + 1,
                    $2, $3, $4, $5, $6, $7
                )"#,
            )
            .bind(session_id)
            .bind(event.event_type.as_str())
            .bind(&event.role)
            .bind(&event.tool_name)
            .bind(&event.tool_use_id)
            .bind(&event.content)
            .bind(&event.metadata)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Mark a session as completed with final stats.
    #[instrument(name = "session.complete", skip_all)]
    pub async fn complete_session(
        &self,
        session_id: SessionId,
        stats: SessionStats,
    ) -> Result<(), SessionError> {
        let result = sqlx::query(
            r#"UPDATE sessions
            SET status = 'completed',
                ended_at = NOW(),
                total_turns = $1,
                total_input_tokens = $2,
                total_output_tokens = $3,
                updated_at = NOW()
            WHERE id = $4"#,
        )
        .bind(stats.total_turns as i32)
        .bind(stats.total_input_tokens as i64)
        .bind(stats.total_output_tokens as i64)
        .bind(session_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(SessionError::SessionNotFound);
        }
        Ok(())
    }

    /// Mark a session as failed with an error message.
    #[instrument(name = "session.fail", skip_all)]
    pub async fn fail_session(
        &self,
        session_id: SessionId,
        error_message: &str,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;

        // Record the error as a session event
        sqlx::query(
            r#"INSERT INTO session_events (session_id, seq, event_type, role, content, metadata)
            VALUES (
                $1,
                COALESCE((SELECT MAX(seq) FROM session_events WHERE session_id = $1), -1) + 1,
                'error', 'system', $2, '{}'
            )"#,
        )
        .bind(session_id)
        .bind(serde_json::json!({ "message": error_message }))
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query(
            r#"UPDATE sessions
            SET status = 'error', ended_at = NOW(), updated_at = NOW()
            WHERE id = $1"#,
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(SessionError::SessionNotFound);
        }

        tx.commit().await?;
        Ok(())
    }

    /// Get all events for a session, ordered by sequence.
    #[instrument(name = "session.get_events", skip_all)]
    pub async fn get_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEventRow>, SessionError> {
        let rows = sqlx::query_as::<_, SessionEventRow>(
            r#"SELECT id, session_id, seq, event_type, role, tool_name, tool_use_id,
                      content, metadata, created_at
            FROM session_events
            WHERE session_id = $1
            ORDER BY seq ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Get display-relevant events for a session (user/assistant/tool/error).
    /// This provides backward compatibility with the old conversation_messages query.
    #[instrument(name = "session.get_display_events", skip_all)]
    pub async fn get_display_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEventRow>, SessionError> {
        let rows = sqlx::query_as::<_, SessionEventRow>(
            r#"SELECT id, session_id, seq, event_type, role, tool_name, tool_use_id,
                      content, metadata, created_at
            FROM session_events
            WHERE session_id = $1
              AND event_type IN ('user', 'assistant', 'tool_call', 'tool_result', 'error')
            ORDER BY seq ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// List sessions for an agent (most recent first).
    #[instrument(name = "session.list_by_agent", skip_all)]
    pub async fn list_by_agent(
        &self,
        agent_id: AgentId,
        limit: i64,
    ) -> Result<Vec<Session>, SessionError> {
        let rows = sqlx::query_as::<_, Session>(
            r#"SELECT id, agent_id, workspace_id, status,
                      started_at, ended_at, total_turns,
                      total_input_tokens, total_output_tokens,
                      created_at, updated_at
            FROM sessions
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

    /// Find a session by its ID.
    #[instrument(name = "session.find", skip_all)]
    pub async fn find_session(&self, id: SessionId) -> Result<Session, SessionError> {
        sqlx::query_as::<_, Session>(
            r#"SELECT id, agent_id, workspace_id, status,
                      started_at, ended_at, total_turns,
                      total_input_tokens, total_output_tokens,
                      created_at, updated_at
            FROM sessions
            WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(SessionError::SessionNotFound)
    }
}
