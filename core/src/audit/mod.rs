pub mod error;
pub mod primitives;

use drua_library::{CommitAttribution, CommitSubjectKind};
use es_entity::context::EventContext;
use tracing::instrument;

use crate::user::Users;

pub use crate::primitives::*;
pub use error::*;
pub use primitives::*;

const AUDIT_CONTEXT_KEY: &str = "audit";

/// Maximum number of bytes persisted for an error message. Anything beyond
/// this is dropped — the goal is debuggability, not storing entire stack
/// traces. Set generously at 4 KiB so typical messages fit without truncation.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

/// Truncate `s` to at most [`MAX_ERROR_MESSAGE_BYTES`] bytes without
/// splitting a UTF-8 code point.
fn truncate_error_message(s: &str) -> String {
    if s.len() <= MAX_ERROR_MESSAGE_BYTES {
        return s.to_owned();
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[derive(Clone)]
pub struct Audit {
    pool: sqlx::PgPool,
}

impl Audit {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Records audit fields into the thread-local `EventContext` under
    /// [`AUDIT_CONTEXT_KEY`]. Call inside an `async { … }.with_event_context(seed)`
    /// block at the request boundary.
    pub fn record_subject(auth: &AuthSubject) {
        Self::update_context(|ctx| match auth {
            AuthSubject::User(user_id) => {
                ctx.acting_user_id = Some(*user_id);
            }
            AuthSubject::ExportedAgent(user_id, _, _) => {
                ctx.acting_user_id = Some(*user_id);
            }
            AuthSubject::Agent(project_id, agent_id, _) => {
                Self::set_resource_id(ctx, "project_id", *project_id);
                ctx.acting_agent_id = Some(*agent_id);
            }
            AuthSubject::AgentOnBehalfOfUser(user_id, project_id, agent_id, _) => {
                ctx.acting_agent_id = Some(*agent_id);
                Self::set_resource_id(ctx, "project_id", *project_id);
                ctx.on_behalf_of_user_id = Some(*user_id);
            }
            AuthSubject::Anonymous => {}
        });
    }

    pub fn record_project_id(project_id: ProjectId) {
        Self::update_context(|ctx| Self::set_resource_id(ctx, "project_id", project_id));
    }

    pub fn record_sandbox_id(sandbox_id: SandboxId) {
        Self::update_context(|ctx| Self::set_resource_id(ctx, "sandbox_id", sandbox_id));
    }

    pub fn record_space_id(space_id: SpaceId) {
        Self::update_context(|ctx| Self::set_resource_id(ctx, "space_id", space_id));
    }

    fn set_resource_id(ctx: &mut AuditContextData, key: &str, id: impl Into<uuid::Uuid>) {
        ctx.resource_ids.insert(
            key.to_owned(),
            serde_json::Value::String(id.into().to_string()),
        );
    }

    pub fn record_interaction_type(itype: InteractionType) {
        Self::update_context(|ctx| ctx.interaction_type = Some(itype));
    }

    /// Record entrypoint (e.g. `"api: POST /projects"`, `"mcp: bash"`).
    /// Called once per request at the boundary layer.
    pub fn record_entrypoint(entrypoint: impl Into<String>) {
        let entrypoint = entrypoint.into();
        Self::update_context(|ctx| ctx.entrypoint = Some(entrypoint));
    }

    pub fn record_action(action: impl Into<String>) {
        let action = action.into();
        Self::update_context(|ctx| ctx.action = Some(action));
    }

    /// Set action only if unset, so boundary layers don't overwrite a
    /// more specific action set by the domain.
    pub fn record_action_if_unset(action: impl Into<String>) {
        let action = action.into();
        Self::update_context(|ctx| {
            if ctx.action.is_none() {
                ctx.action = Some(action);
            }
        });
    }

    pub fn record_agent_id(agent_id: AgentId) {
        Self::update_context(|ctx| Self::set_resource_id(ctx, "agent_id", agent_id));
    }

    pub fn record_workflow_id(workflow_id: WorkflowDefinitionId) {
        Self::update_context(|ctx| Self::set_resource_id(ctx, "workflow_id", workflow_id));
    }

    pub fn record_workflow_run_id(workflow_run_id: WorkflowRunId) {
        Self::update_context(|ctx| Self::set_resource_id(ctx, "workflow_run_id", workflow_run_id));
    }

    pub fn record_outcome(outcome: InteractionOutcome) {
        Self::update_context(|ctx| ctx.outcome = Some(outcome));
    }

    pub fn record_duration(start: std::time::Instant) {
        let ms = start.elapsed().as_millis() as u64;
        Self::update_context(|ctx| ctx.duration_ms = Some(ms));
    }

    pub fn record_tokens(tokens: u64) {
        Self::update_context(|ctx| ctx.tokens_returned = Some(tokens));
    }

    pub fn record_success() {
        Self::update_context(|ctx| ctx.outcome = Some(InteractionOutcome::Success));
    }

    pub fn record_error(message: impl Into<String>) {
        let message = message.into();
        Self::update_context(|ctx| ctx.outcome = Some(InteractionOutcome::Error { message }));
    }

    /// Set outcome only if unset, so the middleware doesn't overwrite a more
    /// specific outcome (e.g. an MCP tool error reported as HTTP 200).
    pub fn record_outcome_if_unset(outcome: InteractionOutcome) {
        Self::update_context(|ctx| {
            if ctx.outcome.is_none() {
                ctx.outcome = Some(outcome);
            }
        });
    }

    pub fn record_metadata(value: serde_json::Value) {
        Self::update_context(|ctx| ctx.metadata = Some(value));
    }

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

    pub fn collect_context() -> Option<AuditContextData> {
        let ec = EventContext::current();
        ec.data().lookup(AUDIT_CONTEXT_KEY).ok().flatten()
    }

    /// Build the [`CommitAttribution`] for the current request and stash
    /// it in the request's [`EventContext`] so library post-persist hooks
    /// (Notes / Skills / Workflows) pick it up.
    ///
    /// JIT-resolves the human's GitHub identity for the
    /// `Co-Authored-By:` line: when an `acting_user_id` or
    /// `on_behalf_of_user_id` is present, looks the user up via `users`
    /// once. Failures degrade to a userless attribution rather than
    /// failing the write.
    #[instrument(name = "audit.commit_attribution", skip_all)]
    pub async fn commit_attribution(users: &Users) -> CommitAttribution {
        let ctx = match Self::collect_context() {
            Some(c) => c,
            None => {
                let attribution = CommitAttribution::library_default();
                attribution.put_in_event_context();
                return attribution;
            }
        };

        let human_user_id = ctx.on_behalf_of_user_id.or(ctx.acting_user_id);
        let human = match human_user_id {
            Some(uid) => match users.find_by_id(uid).await {
                Ok(u) => Some(u),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        user_id = %uid,
                        "commit_attribution: user lookup failed; degrading to userless attribution"
                    );
                    None
                }
            },
            None => None,
        };

        let kind = subject_kind(&ctx);
        let (author_name, author_email, committer_name, committer_email) =
            signatures_for(kind, human.as_ref());

        let mut attribution = CommitAttribution {
            author_name,
            author_email,
            committer_name,
            committer_email,
            kind,
            trailers: Vec::new(),
            co_authored_by: Vec::new(),
        };

        attribution.add_trailer("Drua-Subject-Type", kind.as_trailer_value());

        if let Some(user) = human.as_ref() {
            let (display, email) = github_co_author(user);
            attribution.add_co_authored_by(display, email);
            attribution.add_trailer("Drua-Acting-User", user.id.to_string());
        }

        for (audit_key, trailer_key) in [
            ("project_id", "Drua-Project"),
            ("workflow_id", "Drua-Workflow-Definition"),
            ("workflow_run_id", "Drua-Workflow-Run"),
            ("agent_id", "Drua-Agent"),
            ("sandbox_id", "Drua-Sandbox"),
            ("space_id", "Drua-Space"),
        ] {
            if let Some(value) = ctx.resource_ids.get(audit_key).and_then(|v| v.as_str()) {
                attribution.add_trailer(trailer_key, value);
            }
        }

        if let Some(agent_id) = ctx.acting_agent_id {
            attribution.add_trailer("Drua-Acting-Agent", agent_id.to_string());
        }
        if let Some(action) = ctx.action.as_deref() {
            attribution.add_trailer("Drua-Action", action);
        }
        if let Some(entrypoint) = ctx.entrypoint.as_deref() {
            attribution.add_trailer("Drua-Entrypoint", entrypoint);
        }

        attribution.put_in_event_context();
        attribution
    }

    /// Persist accumulated context as an audit entry. Fire-and-forget;
    /// anonymous subjects and empty contexts are silently skipped.
    pub fn record_from_context(&self) {
        let Some(ctx_data) = Self::collect_context() else {
            return;
        };
        if ctx_data.acting_user_id.is_none() && ctx_data.acting_agent_id.is_none() {
            return;
        }
        let audit = self.clone();
        tokio::spawn(async move {
            if let Err(e) = audit.record(&ctx_data).await {
                tracing::warn!(error = %e, "Failed to record audit entry");
            }
        });
    }

    #[instrument(name = "audit.list_recent", skip_all)]
    pub async fn list_recent(&self, limit: i64) -> Result<Vec<AuditEntry>, AuditError> {
        let rows = sqlx::query_as!(
            AuditEntry,
            r#"SELECT
                id AS "id: AuditEntryId",
                acting_user_id AS "acting_user_id: UserId",
                acting_agent_id AS "acting_agent_id: AgentId",
                on_behalf_of_user_id AS "on_behalf_of_user_id: UserId",
                resource_ids AS "resource_ids: serde_json::Value",
                entrypoint,
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                error,
                error_message,
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

    /// Filter fields are optional and excluded from WHERE when unset.
    /// Strings use `ILIKE`; resource IDs are extracted from `resource_ids` JSONB via `->>`.
    #[instrument(name = "audit.find", skip_all)]
    pub async fn find(&self, query: &AuditLogQuery) -> Result<Vec<AuditEntry>, AuditError> {
        let project_id = query.project_id.map(|id| uuid::Uuid::from(id).to_string());
        let acting_user_id = query.acting_user_id.map(uuid::Uuid::from);
        let acting_agent_id = query.acting_agent_id.map(uuid::Uuid::from);
        let exclude_agent_id = query.exclude_agent_id.map(uuid::Uuid::from);
        let sandbox_id = query.sandbox_id.map(|id| uuid::Uuid::from(id).to_string());
        let entrypoint = query.entrypoint.as_deref();
        let action = query.action.as_deref();
        let outcome = query.outcome.as_deref();
        let error = query.error;
        let limit = query.limit;

        let rows = sqlx::query_as!(
            AuditEntry,
            r#"SELECT
                id AS "id: AuditEntryId",
                acting_user_id AS "acting_user_id: UserId",
                acting_agent_id AS "acting_agent_id: AgentId",
                on_behalf_of_user_id AS "on_behalf_of_user_id: UserId",
                resource_ids AS "resource_ids: serde_json::Value",
                entrypoint,
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                error,
                error_message,
                duration_ms,
                tokens_returned,
                recorded_at
            FROM audit_entries
            WHERE ($1::text IS NULL OR resource_ids->>'project_id' = $1)
              AND ($2::uuid IS NULL OR acting_user_id = $2)
              AND ($3::uuid IS NULL OR acting_agent_id = $3)
              AND ($4::uuid IS NULL OR acting_agent_id IS NULL OR acting_agent_id != $4)
              AND ($5::text IS NULL OR resource_ids->>'sandbox_id' = $5)
              AND ($6::text IS NULL OR entrypoint ILIKE $6)
              AND ($7::text IS NULL OR action ILIKE $7)
              AND ($8::text IS NULL OR outcome ILIKE $8)
              AND ($9::bool IS NULL OR error = $9)
            ORDER BY id DESC
            LIMIT $10"#,
            project_id.as_deref(),
            acting_user_id,
            acting_agent_id,
            exclude_agent_id,
            sandbox_id.as_deref(),
            entrypoint,
            action,
            outcome,
            error,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Persist an audit entry from the given context data.
    ///
    /// Synchronous variant of [`Self::record_from_context`] — callers (and
    /// tests) that need the inserted row should use this directly rather
    /// than the fire-and-forget context helper.
    pub async fn record(&self, ctx: &AuditContextData) -> Result<AuditEntry, AuditError> {
        let acting_user_id = ctx.acting_user_id.map(uuid::Uuid::from);
        let acting_agent_id = ctx.acting_agent_id.map(uuid::Uuid::from);
        let on_behalf_of = ctx.on_behalf_of_user_id.map(uuid::Uuid::from);
        let resource_ids = serde_json::Value::Object(ctx.resource_ids.clone());
        let entrypoint = ctx.entrypoint.clone();
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
        let error_message = match outcome {
            InteractionOutcome::Error { message } => Some(truncate_error_message(message)),
            InteractionOutcome::Success => None,
        };
        let dur = ctx.duration_ms.map(|ms| ms as i64);
        let tokens = ctx.tokens_returned.map(|t| t as i64);

        let row = sqlx::query_as!(
            AuditEntry,
            r#"INSERT INTO audit_entries
                (acting_user_id, acting_agent_id, on_behalf_of_user_id,
                 resource_ids, entrypoint, interaction_type, action, metadata,
                 outcome, error, error_message, duration_ms, tokens_returned)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING
                id AS "id: AuditEntryId",
                acting_user_id AS "acting_user_id: UserId",
                acting_agent_id AS "acting_agent_id: AgentId",
                on_behalf_of_user_id AS "on_behalf_of_user_id: UserId",
                resource_ids AS "resource_ids: serde_json::Value",
                entrypoint,
                interaction_type,
                action,
                metadata AS "metadata: serde_json::Value",
                outcome,
                error,
                error_message,
                duration_ms,
                tokens_returned,
                recorded_at"#,
            acting_user_id,
            acting_agent_id,
            on_behalf_of,
            resource_ids,
            entrypoint,
            itype,
            action,
            metadata,
            out,
            error,
            error_message,
            dur,
            tokens,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }
}

fn subject_kind(ctx: &AuditContextData) -> CommitSubjectKind {
    if ctx.resource_ids.contains_key("workflow_run_id") {
        return CommitSubjectKind::WorkflowAgent;
    }
    match (
        ctx.acting_user_id,
        ctx.acting_agent_id,
        ctx.on_behalf_of_user_id,
    ) {
        (_, Some(_), Some(_)) => CommitSubjectKind::AgentOnBehalfOfUser,
        (_, Some(_), None) => CommitSubjectKind::WorkflowAgent,
        (Some(_), None, _) => CommitSubjectKind::User,
        (None, None, Some(_)) => CommitSubjectKind::AgentOnBehalfOfUser,
        (None, None, None) => CommitSubjectKind::System,
    }
}

fn signatures_for(
    kind: CommitSubjectKind,
    human: Option<&crate::user::User>,
) -> (String, String, String, String) {
    use drua_library::attribution::{
        ADMIN_BOT_EMAIL, ADMIN_BOT_NAME, AGENT_BOT_EMAIL, AGENT_BOT_NAME, LIBRARY_BOT_EMAIL,
        LIBRARY_BOT_NAME, SYSTEM_BOT_EMAIL, SYSTEM_BOT_NAME, WORKFLOW_BOT_EMAIL, WORKFLOW_BOT_NAME,
    };
    match kind {
        CommitSubjectKind::System => (
            SYSTEM_BOT_NAME.to_owned(),
            SYSTEM_BOT_EMAIL.to_owned(),
            SYSTEM_BOT_NAME.to_owned(),
            SYSTEM_BOT_EMAIL.to_owned(),
        ),
        CommitSubjectKind::Library => (
            LIBRARY_BOT_NAME.to_owned(),
            LIBRARY_BOT_EMAIL.to_owned(),
            LIBRARY_BOT_NAME.to_owned(),
            LIBRARY_BOT_EMAIL.to_owned(),
        ),
        CommitSubjectKind::User | CommitSubjectKind::ExportedAgent => {
            let (display, email) = human
                .map(github_co_author)
                .unwrap_or_else(|| (ADMIN_BOT_NAME.to_owned(), ADMIN_BOT_EMAIL.to_owned()));
            (
                format!("{display} (via drua)"),
                email,
                ADMIN_BOT_NAME.to_owned(),
                ADMIN_BOT_EMAIL.to_owned(),
            )
        }
        CommitSubjectKind::AgentOnBehalfOfUser => {
            let (display, email) = human
                .map(github_co_author)
                .unwrap_or_else(|| (AGENT_BOT_NAME.to_owned(), AGENT_BOT_EMAIL.to_owned()));
            (
                format!("{display} (via drua)"),
                email,
                AGENT_BOT_NAME.to_owned(),
                AGENT_BOT_EMAIL.to_owned(),
            )
        }
        CommitSubjectKind::WorkflowAgent => (
            WORKFLOW_BOT_NAME.to_owned(),
            WORKFLOW_BOT_EMAIL.to_owned(),
            WORKFLOW_BOT_NAME.to_owned(),
            WORKFLOW_BOT_EMAIL.to_owned(),
        ),
    }
}

/// Render a `(display_name, email)` pair for a `Co-Authored-By:` line.
/// Email keys on the immutable GitHub user id (form
/// `<id>+<username>@users.noreply.github.com`) so renames don't break
/// avatar lookup.
fn github_co_author(user: &crate::user::User) -> (String, String) {
    let display = user
        .name
        .clone()
        .or_else(|| user.github_username.clone())
        .unwrap_or_else(|| format!("github:{}", user.github_id));
    let username = user.github_username.as_deref().unwrap_or("anonymous");
    let email = format!(
        "{id}+{username}@users.noreply.github.com",
        id = user.github_id,
        username = username,
    );
    (display, email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_error_message_short_passthrough() {
        let s = "boom";
        assert_eq!(truncate_error_message(s), "boom");
    }

    #[test]
    fn truncate_error_message_caps_at_limit() {
        let s = "x".repeat(MAX_ERROR_MESSAGE_BYTES + 100);
        let out = truncate_error_message(&s);
        assert_eq!(out.len(), MAX_ERROR_MESSAGE_BYTES);
    }

    #[test]
    fn truncate_error_message_respects_utf8_boundaries() {
        // 4-byte char (😀 = 0xF0 0x9F 0x98 0x80) straddling the cap.
        let prefix_len = MAX_ERROR_MESSAGE_BYTES - 2;
        let mut s = "a".repeat(prefix_len);
        s.push('😀');
        s.push_str("tail");
        let out = truncate_error_message(&s);
        // Cut should land on the char boundary before the emoji.
        assert_eq!(out.len(), prefix_len);
        assert!(out.is_char_boundary(out.len()));
    }
}
