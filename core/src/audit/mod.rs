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

    /// Decompose the authenticated subject into explicit audit fields.
    pub fn record_subject(auth: &AuthSubject) {
        Self::update_context(|ctx| match auth {
            AuthSubject::User(user_id) => {
                ctx.acting_user_id = Some(*user_id);
            }
            AuthSubject::ExportedAgent(user_id, _, _) => {
                ctx.acting_user_id = Some(*user_id);
            }
            AuthSubject::Agent(agent_id, _) => {
                ctx.workspace_id = auth.workspace_id();
                ctx.acting_agent_id = Some(*agent_id);
            }
            AuthSubject::AgentOnBehalfOfUser(agent_id, user_id, _) => {
                ctx.acting_agent_id = Some(*agent_id);
                ctx.workspace_id = auth.workspace_id();
                ctx.on_behalf_of_user_id = Some(*user_id);
            }
            AuthSubject::Anonymous => {}
        });
    }

    /// Record the workspace that scopes this interaction.
    pub fn record_workspace_id(workspace_id: WorkspaceId) {
        Self::update_context(|ctx| ctx.workspace_id = Some(workspace_id));
    }

    /// Record the sandbox targeted by this interaction.
    pub fn record_sandbox_id(sandbox_id: SandboxId) {
        Self::update_context(|ctx| ctx.sandbox_id = Some(sandbox_id));
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

    /// Set the outcome only if no inner handler has recorded one yet.
    /// Used by the middleware so it doesn't overwrite a more specific
    /// outcome (e.g. an MCP tool error reported as HTTP 200).
    pub fn record_outcome_if_unset(outcome: InteractionOutcome) {
        Self::update_context(|ctx| {
            if ctx.outcome.is_none() {
                ctx.outcome = Some(outcome);
            }
        });
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
        // Skip anonymous — nothing to attribute.
        if ctx_data.acting_user_id.is_none() && ctx_data.acting_agent_id.is_none() {
            return;
        }
        let audit = self.clone();
        tokio::spawn(async move {
            if let Err(e) = audit.insert(&ctx_data).await {
                tracing::warn!(error = %e, "Failed to record audit entry");
            }
        });
    }

    /// List recent audit entries (most recent first).
    #[instrument(name = "audit.list_recent", skip_all)]
    pub async fn list_recent(&self, limit: i64) -> Result<Vec<AuditEntry>, AuditError> {
        let rows = sqlx::query_as!(
            AuditEntry,
            r#"SELECT
                id AS "id: AuditEntryId",
                acting_user_id AS "acting_user_id: UserId",
                workspace_id AS "workspace_id: WorkspaceId",
                acting_agent_id AS "acting_agent_id: AgentId",
                on_behalf_of_user_id AS "on_behalf_of_user_id: UserId",
                sandbox_id AS "sandbox_id: SandboxId",
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                error,
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

    /// Query audit entries using the provided filter criteria.
    ///
    /// All filter fields are optional — unset fields are excluded from the
    /// WHERE clause. String fields use `ILIKE` for fuzzy matching.
    #[instrument(name = "audit.find", skip_all)]
    pub async fn find(&self, query: &AuditLogQuery) -> Result<Vec<AuditEntry>, AuditError> {
        let workspace_id = query.workspace_id.map(uuid::Uuid::from);
        let acting_user_id = query.acting_user_id.map(uuid::Uuid::from);
        let acting_agent_id = query.acting_agent_id.map(uuid::Uuid::from);
        let exclude_agent_id = query.exclude_agent_id.map(uuid::Uuid::from);
        let sandbox_id = query.sandbox_id.map(uuid::Uuid::from);
        let action = query.action.as_deref();
        let outcome = query.outcome.as_deref();
        let error = query.error;
        let limit = query.limit;

        let rows = sqlx::query_as!(
            AuditEntry,
            r#"SELECT
                id AS "id: AuditEntryId",
                acting_user_id AS "acting_user_id: UserId",
                workspace_id AS "workspace_id: WorkspaceId",
                acting_agent_id AS "acting_agent_id: AgentId",
                on_behalf_of_user_id AS "on_behalf_of_user_id: UserId",
                sandbox_id AS "sandbox_id: SandboxId",
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                error,
                duration_ms,
                tokens_returned,
                recorded_at
            FROM audit_entries
            WHERE ($1::uuid IS NULL OR workspace_id = $1)
              AND ($2::uuid IS NULL OR acting_user_id = $2)
              AND ($3::uuid IS NULL OR acting_agent_id = $3)
              AND ($4::uuid IS NULL OR acting_agent_id IS NULL OR acting_agent_id != $4)
              AND ($5::uuid IS NULL OR sandbox_id = $5)
              AND ($6::text IS NULL OR action ILIKE $6)
              AND ($7::text IS NULL OR outcome ILIKE $7)
              AND ($8::bool IS NULL OR error = $8)
            ORDER BY id DESC
            LIMIT $9"#,
            workspace_id,
            acting_user_id,
            acting_agent_id,
            exclude_agent_id,
            sandbox_id,
            action,
            outcome,
            error,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn insert(&self, ctx: &AuditContextData) -> Result<AuditEntry, AuditError> {
        let acting_user_id = ctx.acting_user_id.map(uuid::Uuid::from);
        let workspace_id = ctx.workspace_id.map(uuid::Uuid::from);
        let acting_agent_id = ctx.acting_agent_id.map(uuid::Uuid::from);
        let on_behalf_of = ctx.on_behalf_of_user_id.map(uuid::Uuid::from);
        let sandbox_id = ctx.sandbox_id.map(uuid::Uuid::from);
        let itype = ctx
            .interaction_type
            .as_ref()
            .unwrap_or(&InteractionType::ApiCall)
            .to_string();
        let action = ctx.action.clone().unwrap_or_default();
        let metadata = ctx.metadata.clone().unwrap_or(serde_json::json!({}));
        let outcome = ctx.outcome.as_ref().unwrap_or(&InteractionOutcome::Success);
        let out = outcome.to_string();
        let error = matches!(outcome, InteractionOutcome::Error { .. });
        let dur = ctx.duration_ms.map(|ms| ms as i64);
        let tokens = ctx.tokens_returned.map(|t| t as i64);

        let row = sqlx::query_as!(
            AuditEntry,
            r#"INSERT INTO audit_entries
                (acting_user_id, workspace_id, acting_agent_id, on_behalf_of_user_id,
                 sandbox_id, interaction_type, action, metadata, outcome, error,
                 duration_ms, tokens_returned)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            RETURNING
                id AS "id: AuditEntryId",
                acting_user_id AS "acting_user_id: UserId",
                workspace_id AS "workspace_id: WorkspaceId",
                acting_agent_id AS "acting_agent_id: AgentId",
                on_behalf_of_user_id AS "on_behalf_of_user_id: UserId",
                sandbox_id AS "sandbox_id: SandboxId",
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                error,
                duration_ms,
                tokens_returned,
                recorded_at"#,
            acting_user_id,
            workspace_id,
            acting_agent_id,
            on_behalf_of,
            sandbox_id,
            itype,
            action,
            metadata,
            out,
            error,
            dur,
            tokens,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }
}
