pub mod error;
pub mod primitives;

use tracing::instrument;

pub use error::*;
pub use primitives::*;

#[derive(Clone)]
pub struct Audit {
    pool: sqlx::PgPool,
}

impl Audit {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Record an API interaction.
    #[instrument(name = "audit.record_api_call", skip_all)]
    pub async fn record_api_call(
        &self,
        subject: impl Into<AuditSubject>,
        action: impl Into<String>,
        metadata: serde_json::Value,
        outcome: InteractionOutcome,
        duration_ms: Option<u64>,
    ) -> Result<AuditEntry, AuditError> {
        self.insert(
            subject.into(),
            InteractionType::ApiCall,
            action.into(),
            metadata,
            outcome,
            duration_ms,
            None,
        )
        .await
    }

    /// Record an MCP tool call interaction.
    #[instrument(name = "audit.record_mcp_call", skip_all)]
    pub async fn record_mcp_call(
        &self,
        subject: impl Into<AuditSubject>,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        outcome: InteractionOutcome,
        duration_ms: Option<u64>,
        tokens_returned: Option<u64>,
    ) -> Result<AuditEntry, AuditError> {
        let metadata = serde_json::json!({
            "tool_name": tool_name,
            "arguments": arguments,
        });
        self.insert(
            subject.into(),
            InteractionType::McpCall,
            tool_name.to_string(),
            metadata,
            outcome,
            duration_ms,
            tokens_returned,
        )
        .await
    }

    /// List recent audit entries (most recent first).
    #[instrument(name = "audit.list_recent", skip_all)]
    pub async fn list_recent(&self, limit: i64) -> Result<Vec<AuditEntry>, AuditError> {
        let rows = sqlx::query_as!(
            AuditEntry,
            r#"SELECT
                id AS "id: AuditEntryId",
                subject,
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                duration_ms,
                tokens_returned,
                recorded_at
            FROM audit_entries
            ORDER BY id DESC
            LIMIT $1"#,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Find audit entries by subject string (most recent first).
    #[instrument(name = "audit.find_by_subject", skip_all)]
    pub async fn find_by_subject(
        &self,
        subject: &str,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        let rows = sqlx::query_as!(
            AuditEntry,
            r#"SELECT
                id AS "id: AuditEntryId",
                subject,
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                duration_ms,
                tokens_returned,
                recorded_at
            FROM audit_entries
            WHERE subject = $1
            ORDER BY id DESC
            LIMIT $2"#,
            subject,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        &self,
        subject: AuditSubject,
        interaction_type: InteractionType,
        action: String,
        metadata: serde_json::Value,
        outcome: InteractionOutcome,
        duration_ms: Option<u64>,
        tokens_returned: Option<u64>,
    ) -> Result<AuditEntry, AuditError> {
        let sub = subject.to_string();
        let itype = interaction_type.to_string();
        let out = outcome.to_string();
        let dur = duration_ms.map(|ms| ms as i64);
        let tokens = tokens_returned.map(|t| t as i64);

        let row = sqlx::query_as!(
            AuditEntry,
            r#"INSERT INTO audit_entries (subject, interaction_type, action, metadata, outcome, duration_ms, tokens_returned)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING
                id AS "id: AuditEntryId",
                subject,
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                duration_ms,
                tokens_returned,
                recorded_at"#,
            sub,
            itype,
            action,
            metadata,
            out,
            dur,
            tokens,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }
}
