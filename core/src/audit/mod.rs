pub mod error;
pub mod primitives;

use es_entity::context::EventContext;
use tracing::instrument;

pub use crate::primitives::*;
pub use error::*;
pub use primitives::*;

/// Well-known key under which [`AuditContextData`] is stored in the
/// [`EventContext`].
const AUDIT_CONTEXT_KEY: &str = "audit";

#[derive(Clone)]
pub struct Audit {
    pool: sqlx::PgPool,
}

impl Audit {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    // ------------------------------------------------------------------
    // Type-safe context accumulation
    //
    // These associated functions record fields into the thread-local
    // `EventContext` under [`AUDIT_CONTEXT_KEY`]. Call them inside an
    // `async { … }.with_event_context(seed)` block at the request
    // boundary so the context is properly propagated.
    // ------------------------------------------------------------------

    /// Record the authenticated subject.
    pub fn record_subject(auth: &AuthSubject) {
        let subject: AuditSubject = auth.into();
        Self::update_context(|ctx| ctx.subject = Some(subject));
    }

    /// Record the interaction type (API call, MCP call, …).
    pub fn record_interaction_type(itype: InteractionType) {
        Self::update_context(|ctx| ctx.interaction_type = Some(itype));
    }

    /// Record the action label (e.g. `"POST /workspaces"`).
    pub fn record_action(action: impl Into<String>) {
        let action = action.into();
        Self::update_context(|ctx| ctx.action = Some(action));
    }

    /// Derive and record the outcome from an HTTP status code.
    pub fn record_outcome(outcome: InteractionOutcome) {
        Self::update_context(|ctx| ctx.outcome = Some(outcome));
    }

    /// Compute elapsed time from `start` and record it.
    pub fn record_duration(start: std::time::Instant) {
        let ms = start.elapsed().as_millis() as u64;
        Self::update_context(|ctx| ctx.duration_ms = Some(ms));
    }

    /// Record the estimated token count of the response.
    pub fn record_tokens(tokens: u64) {
        Self::update_context(|ctx| ctx.tokens_returned = Some(tokens));
    }

    /// Record a successful outcome.
    pub fn record_success() {
        Self::update_context(|ctx| ctx.outcome = Some(InteractionOutcome::Success));
    }

    /// Record an error outcome with a message.
    pub fn record_error(message: impl Into<String>) {
        let message = message.into();
        Self::update_context(|ctx| ctx.outcome = Some(InteractionOutcome::Error { message }));
    }

    /// Merge arbitrary metadata into the context.
    pub fn record_metadata(value: serde_json::Value) {
        Self::update_context(|ctx| ctx.metadata = Some(value));
    }

    /// Read-then-modify-then-write the [`AuditContextData`] stored in the
    /// current [`EventContext`]. Creates a default if none exists yet.
    fn update_context(f: impl FnOnce(&mut AuditContextData)) {
        let mut ec = EventContext::current();
        let mut data: AuditContextData = ec
            .data()
            .lookup(AUDIT_CONTEXT_KEY)
            .ok()
            .flatten()
            .unwrap_or_default();
        f(&mut data);
        ec.insert(AUDIT_CONTEXT_KEY, &data).unwrap_or_default();
    }

    /// Collect the accumulated [`AuditContextData`] from the current
    /// [`EventContext`]. Returns `None` if nothing was recorded.
    pub fn collect_context() -> Option<AuditContextData> {
        let ec = EventContext::current();
        ec.data().lookup(AUDIT_CONTEXT_KEY).ok().flatten()
    }

    /// Collect the accumulated context and persist it as an audit entry.
    ///
    /// Persistence is fire-and-forget (`tokio::spawn`) so this never blocks
    /// the caller. Anonymous subjects and empty contexts are silently skipped.
    pub fn record_from_context(&self) {
        let Some(ctx_data) = Self::collect_context() else {
            return;
        };
        let subject = match ctx_data.subject {
            Some(ref s) if !matches!(s, AuditSubject::Anonymous) => s.clone(),
            _ => return,
        };
        let audit = self.clone();
        tokio::spawn(async move {
            let itype = ctx_data
                .interaction_type
                .unwrap_or(InteractionType::ApiCall);
            if let Err(e) = audit
                .insert(
                    subject,
                    itype,
                    ctx_data.action.unwrap_or_default(),
                    ctx_data.metadata.unwrap_or(serde_json::json!({})),
                    ctx_data.outcome.unwrap_or(InteractionOutcome::Success),
                    ctx_data.duration_ms,
                    ctx_data.tokens_returned,
                )
                .await
            {
                tracing::warn!(error = %e, "Failed to record audit entry");
            }
        });
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
