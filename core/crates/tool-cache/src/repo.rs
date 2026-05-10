use sqlx::PgPool;

use crate::entity::*;

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
        let agent_uuid: Option<uuid::Uuid> = new.owner.agent_id.map(Into::into);
        let user_uuid: Option<uuid::Uuid> = new.owner.user_id.map(Into::into);

        let row = sqlx::query!(
            r#"
            INSERT INTO tool_invocations (
                id, agent_id, user_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, original_structured,
                exit_code, duration_ms, started_at, root_path
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            RETURNING created_at
            "#,
            id_uuid,
            agent_uuid,
            user_uuid,
            new.tool_name,
            new.args,
            new.args_hash,
            new.classifier,
            new.summary,
            new.raw_text,
            new.raw_size_bytes,
            new.original_structured,
            new.exit_code,
            new.duration_ms,
            new.started_at,
            new.root_path,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(ToolInvocation {
            id,
            owner: new.owner,
            tool_name: new.tool_name,
            args: new.args,
            args_hash: new.args_hash,
            classifier: new.classifier,
            summary: new.summary,
            raw_text: new.raw_text,
            raw_size_bytes: new.raw_size_bytes,
            original_structured: new.original_structured,
            exit_code: new.exit_code,
            duration_ms: new.duration_ms,
            started_at: new.started_at,
            created_at: row.created_at,
            root_path: new.root_path,
        })
    }

    pub async fn find_by_id(&self, id: ToolInvocationId) -> Result<ToolInvocation, sqlx::Error> {
        let id_uuid: uuid::Uuid = id.into();
        let row = sqlx::query!(
            r#"
            SELECT
                id, agent_id, user_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, original_structured,
                exit_code, duration_ms, started_at, created_at, root_path
            FROM tool_invocations
            WHERE id = $1
            "#,
            id_uuid,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(hydrate_row(
            row.id,
            row.agent_id,
            row.user_id,
            row.tool_name,
            row.args,
            row.args_hash,
            row.classifier,
            row.summary,
            row.raw_text,
            row.raw_size_bytes,
            row.original_structured,
            row.exit_code,
            row.duration_ms,
            row.started_at,
            row.created_at,
            row.root_path,
        ))
    }

    pub async fn find_latest_by_args_hash(
        &self,
        owner: InvocationOwner,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, sqlx::Error> {
        if let Some(agent_id) = owner.agent_id {
            self.find_latest_by_agent(agent_id.into(), args_hash).await
        } else if let Some(user_id) = owner.user_id {
            self.find_latest_by_user(user_id.into(), args_hash).await
        } else {
            Ok(None)
        }
    }

    async fn find_latest_by_agent(
        &self,
        agent_id: uuid::Uuid,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT
                id, agent_id, user_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, original_structured,
                exit_code, duration_ms, started_at, created_at, root_path
            FROM tool_invocations
            WHERE agent_id = $1 AND args_hash = $2
            ORDER BY created_at DESC
            LIMIT 1
            "#,
            agent_id,
            args_hash,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            hydrate_row(
                r.id,
                r.agent_id,
                r.user_id,
                r.tool_name,
                r.args,
                r.args_hash,
                r.classifier,
                r.summary,
                r.raw_text,
                r.raw_size_bytes,
                r.original_structured,
                r.exit_code,
                r.duration_ms,
                r.started_at,
                r.created_at,
                r.root_path,
            )
        }))
    }

    async fn find_latest_by_user(
        &self,
        user_id: uuid::Uuid,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT
                id, agent_id, user_id, tool_name, args, args_hash, classifier,
                summary, raw_text, raw_size_bytes, original_structured,
                exit_code, duration_ms, started_at, created_at, root_path
            FROM tool_invocations
            WHERE user_id = $1 AND args_hash = $2
            ORDER BY created_at DESC
            LIMIT 1
            "#,
            user_id,
            args_hash,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            hydrate_row(
                r.id,
                r.agent_id,
                r.user_id,
                r.tool_name,
                r.args,
                r.args_hash,
                r.classifier,
                r.summary,
                r.raw_text,
                r.raw_size_bytes,
                r.original_structured,
                r.exit_code,
                r.duration_ms,
                r.started_at,
                r.created_at,
                r.root_path,
            )
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn hydrate_row(
    id: uuid::Uuid,
    agent_id: Option<uuid::Uuid>,
    user_id: Option<uuid::Uuid>,
    tool_name: String,
    args: serde_json::Value,
    args_hash: Vec<u8>,
    classifier: String,
    summary: serde_json::Value,
    raw_text: String,
    raw_size_bytes: i64,
    original_structured: Option<serde_json::Value>,
    exit_code: Option<i32>,
    duration_ms: i32,
    started_at: chrono::DateTime<chrono::Utc>,
    created_at: chrono::DateTime<chrono::Utc>,
    root_path: String,
) -> ToolInvocation {
    let owner = InvocationOwner {
        agent_id: agent_id.map(Into::into),
        user_id: user_id.map(Into::into),
    };
    ToolInvocation {
        id: ToolInvocationId::from(id),
        owner,
        tool_name,
        args,
        args_hash,
        classifier,
        summary,
        raw_text,
        raw_size_bytes,
        original_structured,
        exit_code,
        duration_ms,
        started_at,
        created_at,
        root_path,
    }
}
