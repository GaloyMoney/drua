use crate::error::ToolCachingError;
use crate::primitives::{QueryStructure, ToolCallOwnerId, ToolCallSummary, ToolInvocationId};

/// Hydrated form of a persisted invocation. Carries the parsed root
/// (`query_structure`) — the fetch tool navigates it via json-path.
pub struct StoredInvocation {
    #[allow(dead_code)]
    pub id: ToolInvocationId,
    pub query_structure: QueryStructure,
    pub summary: ToolCallSummary,
    pub original_structured: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct ToolCacheRepo {
    pool: sqlx::PgPool,
}

impl ToolCacheRepo {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn persist(
        &self,
        id: ToolInvocationId,
        owner_id: ToolCallOwnerId,
        tool_name: &str,
        args: &serde_json::Value,
        query_structure: &QueryStructure,
        summary: &ToolCallSummary,
        original_text: &str,
        original_structured: Option<&serde_json::Value>,
    ) -> Result<(), ToolCachingError> {
        let summary_json = serde_json::to_value(summary)?;
        sqlx::query!(
            r#"INSERT INTO tool_invocations
                (id, owner_id, tool_name, args, query_structure, summary,
                 raw_text, original_structured)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
            id as ToolInvocationId,
            owner_id as ToolCallOwnerId,
            tool_name,
            args,
            &query_structure.root,
            summary_json,
            original_text,
            original_structured,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Look up an invocation, scoped to its owner so an agent/user can
    /// only fetch their own results. Returns `InvocationNotFound` for
    /// any mismatch — never leaks existence to a non-owner.
    pub async fn find_by_id(
        &self,
        id: ToolInvocationId,
        owner_id: ToolCallOwnerId,
    ) -> Result<StoredInvocation, ToolCachingError> {
        let row = sqlx::query!(
            r#"SELECT query_structure, summary, original_structured
               FROM tool_invocations
               WHERE id = $1 AND owner_id = $2"#,
            id as ToolInvocationId,
            owner_id as ToolCallOwnerId,
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ToolCachingError::InvocationNotFound)?;

        let summary: ToolCallSummary = serde_json::from_value(row.summary)?;
        Ok(StoredInvocation {
            id,
            query_structure: QueryStructure {
                root: row.query_structure,
            },
            summary,
            original_structured: row.original_structured,
        })
    }
}
