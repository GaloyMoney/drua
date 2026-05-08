use sqlx::PgPool;

use crate::primitives::{AgentId, ToolInvocationId};

use super::entity::*;

#[derive(Clone)]
pub struct ToolInvocationRepo {
    pool: PgPool,
}

impl ToolInvocationRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    pub async fn create(&self, new: NewToolInvocation) -> Result<ToolInvocation, sqlx::Error> {
        let id = ToolInvocationId::new();
        let id_uuid: uuid::Uuid = id.into();
        let agent_uuid: uuid::Uuid = new.agent_id.into();

        let row = sqlx::query!(
            r#"
            INSERT INTO tool_invocations (
                id, agent_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, exit_code,
                duration_ms, started_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            RETURNING created_at
            "#,
            id_uuid,
            agent_uuid,
            new.tool_name,
            new.args,
            new.args_hash,
            new.classifier,
            new.summary,
            new.raw_text,
            new.raw_size_bytes,
            new.exit_code,
            new.duration_ms,
            new.started_at,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(ToolInvocation {
            id,
            agent_id: new.agent_id,
            tool_name: new.tool_name,
            args: new.args,
            args_hash: new.args_hash,
            classifier: new.classifier,
            summary: new.summary,
            raw_text: new.raw_text,
            raw_size_bytes: new.raw_size_bytes,
            exit_code: new.exit_code,
            duration_ms: new.duration_ms,
            started_at: new.started_at,
            created_at: row.created_at,
        })
    }

    pub async fn find_by_id(&self, id: ToolInvocationId) -> Result<ToolInvocation, sqlx::Error> {
        let id_uuid: uuid::Uuid = id.into();
        let row = sqlx::query!(
            r#"
            SELECT
                id, agent_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, exit_code,
                duration_ms, started_at, created_at
            FROM tool_invocations
            WHERE id = $1
            "#,
            id_uuid,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(ToolInvocation {
            id: ToolInvocationId::from(row.id),
            agent_id: AgentId::from(row.agent_id),
            tool_name: row.tool_name,
            args: row.args,
            args_hash: row.args_hash,
            classifier: row.classifier,
            summary: row.summary,
            raw_text: row.raw_text,
            raw_size_bytes: row.raw_size_bytes,
            exit_code: row.exit_code,
            duration_ms: row.duration_ms,
            started_at: row.started_at,
            created_at: row.created_at,
        })
    }

    /// Most-recent matching invocation for `(agent_id, args_hash)` — the
    /// cache-aware-diff probe. Returns the full row so the caller can
    /// compare summaries without a second round-trip.
    pub async fn find_latest_by_args_hash(
        &self,
        agent_id: AgentId,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, sqlx::Error> {
        let agent_uuid: uuid::Uuid = agent_id.into();
        let row = sqlx::query!(
            r#"
            SELECT
                id, agent_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, exit_code,
                duration_ms, started_at, created_at
            FROM tool_invocations
            WHERE agent_id = $1 AND args_hash = $2
            ORDER BY created_at DESC
            LIMIT 1
            "#,
            agent_uuid,
            args_hash,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ToolInvocation {
            id: ToolInvocationId::from(r.id),
            agent_id: AgentId::from(r.agent_id),
            tool_name: r.tool_name,
            args: r.args,
            args_hash: r.args_hash,
            classifier: r.classifier,
            summary: r.summary,
            raw_text: r.raw_text,
            raw_size_bytes: r.raw_size_bytes,
            exit_code: r.exit_code,
            duration_ms: r.duration_ms,
            started_at: r.started_at,
            created_at: r.created_at,
        }))
    }
}
