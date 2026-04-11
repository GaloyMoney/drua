pub mod error;
pub mod primitives;

use tracing::instrument;

pub use error::*;
pub use primitives::*;

use crate::primitives::*;

/// Flat SQL service for session recording (not event-sourced).
///
/// Sessions capture the full interaction history between a user and an agent,
/// including tool calls, tool results, and raw harness events.
#[derive(Clone)]
pub struct Sessions {
    pool: sqlx::PgPool,
}

impl Sessions {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Create a new session for the given agent and user.
    #[instrument(name = "session.create", skip_all)]
    pub async fn create_session(
        &self,
        agent_id: AgentId,
        user_id: UserId,
        workspace_id: WorkspaceId,
        runtime: SessionRuntime,
        model: Option<&str>,
    ) -> Result<Session, SessionError> {
        let id = SessionId::new();
        let row = sqlx::query_as::<_, Session>(
            r#"INSERT INTO sessions (id, agent_id, user_id, workspace_id, runtime, model)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, agent_id, user_id, workspace_id, claude_session_id,
                      runtime, model, status, started_at, ended_at,
                      total_turns, total_input_tokens, total_output_tokens,
                      cost_usd, duration_ms, created_at, updated_at"#,
        )
        .bind(id)
        .bind(agent_id)
        .bind(user_id)
        .bind(workspace_id)
        .bind(runtime.as_str())
        .bind(model)
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
        let role = event.event_type.role();
        sqlx::query(
            r#"INSERT INTO session_events (session_id, seq, event_type, role, tool_name, tool_use_id, content, metadata, raw_event)
            VALUES (
                $1,
                COALESCE((SELECT MAX(seq) FROM session_events WHERE session_id = $1), -1) + 1,
                $2, $3, $4, $5, $6, $7, $8
            )"#,
        )
        .bind(session_id)
        .bind(event.event_type.as_str())
        .bind(role)
        .bind(&event.tool_name)
        .bind(&event.tool_use_id)
        .bind(&event.content)
        .bind(&event.metadata)
        .bind(&event.raw_event)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append multiple events to a session in a single transaction.
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
            let role = event.event_type.role();
            sqlx::query(
                r#"INSERT INTO session_events (session_id, seq, event_type, role, tool_name, tool_use_id, content, metadata, raw_event)
                VALUES (
                    $1,
                    COALESCE((SELECT MAX(seq) FROM session_events WHERE session_id = $1), -1) + 1,
                    $2, $3, $4, $5, $6, $7, $8
                )"#,
            )
            .bind(session_id)
            .bind(event.event_type.as_str())
            .bind(role)
            .bind(&event.tool_name)
            .bind(&event.tool_use_id)
            .bind(&event.content)
            .bind(&event.metadata)
            .bind(&event.raw_event)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Mark a session as completed with final statistics.
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
                claude_session_id = COALESCE($1, claude_session_id),
                total_turns = $2,
                total_input_tokens = $3,
                total_output_tokens = $4,
                cost_usd = $5,
                duration_ms = $6,
                updated_at = NOW()
            WHERE id = $7"#,
        )
        .bind(&stats.claude_session_id)
        .bind(stats.total_turns)
        .bind(stats.total_input_tokens)
        .bind(stats.total_output_tokens)
        .bind(stats.cost_usd)
        .bind(stats.duration_ms)
        .bind(session_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(SessionError::SessionNotFound);
        }
        Ok(())
    }

    /// Mark a session as failed.
    #[instrument(name = "session.fail", skip_all)]
    pub async fn fail_session(&self, session_id: SessionId) -> Result<(), SessionError> {
        let result = sqlx::query(
            r#"UPDATE sessions
            SET status = 'failed', ended_at = NOW(), updated_at = NOW()
            WHERE id = $1"#,
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(SessionError::SessionNotFound);
        }
        Ok(())
    }

    /// Find a session by its ID.
    #[instrument(name = "session.find", skip_all)]
    pub async fn find_session(&self, id: SessionId) -> Result<Session, SessionError> {
        sqlx::query_as::<_, Session>(
            r#"SELECT id, agent_id, user_id, workspace_id, claude_session_id,
                      runtime, model, status, started_at, ended_at,
                      total_turns, total_input_tokens, total_output_tokens,
                      cost_usd, duration_ms, created_at, updated_at
            FROM sessions WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(SessionError::SessionNotFound)
    }

    /// List sessions for an agent (most recent first).
    #[instrument(name = "session.list_by_agent", skip_all)]
    pub async fn list_by_agent(
        &self,
        agent_id: AgentId,
        limit: i64,
    ) -> Result<Vec<Session>, SessionError> {
        let rows = sqlx::query_as::<_, Session>(
            r#"SELECT id, agent_id, user_id, workspace_id, claude_session_id,
                      runtime, model, status, started_at, ended_at,
                      total_turns, total_input_tokens, total_output_tokens,
                      cost_usd, duration_ms, created_at, updated_at
            FROM sessions WHERE agent_id = $1
            ORDER BY created_at DESC LIMIT $2"#,
        )
        .bind(agent_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Get all events for a session, ordered by sequence.
    #[instrument(name = "session.get_events", skip_all)]
    pub async fn get_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEventRow>, SessionError> {
        let rows = sqlx::query_as::<_, SessionEventRow>(
            r#"SELECT id, session_id, seq, event_type, role, tool_name, tool_use_id,
                      content, metadata, raw_event, created_at
            FROM session_events WHERE session_id = $1
            ORDER BY seq ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Get chat-compatible events for a session (user messages, assistant text,
    /// tool calls, tool results, errors) — suitable for the dashboard chat UI.
    #[instrument(name = "session.get_chat_messages", skip_all)]
    pub async fn get_chat_messages(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEventRow>, SessionError> {
        let rows = sqlx::query_as::<_, SessionEventRow>(
            r#"SELECT id, session_id, seq, event_type, role, tool_name, tool_use_id,
                      content, metadata, raw_event, created_at
            FROM session_events WHERE session_id = $1
              AND event_type IN ('user_message', 'assistant_text', 'tool_call', 'tool_result', 'error')
            ORDER BY seq ASC"#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
