mod entity;
pub mod error;
pub mod primitives;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::AuditEntry;
use entity::*;
pub use error::*;
pub use primitives::*;
use repo::*;

use crate::auth::AuthContext;

impl From<&AuthContext> for AuditSubject {
    fn from(ctx: &AuthContext) -> Self {
        match ctx {
            AuthContext::User(user_id) => AuditSubject::User { user_id: *user_id },
            AuthContext::Agent(agent_id, user_id) => AuditSubject::Agent {
                agent_id: *agent_id,
                user_id: *user_id,
            },
            AuthContext::Anonymous => AuditSubject::Anonymous,
        }
    }
}

#[derive(Clone)]
pub struct AuditEntries {
    repo: AuditEntryRepo,
}

impl AuditEntries {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            repo: AuditEntryRepo::new(pool),
        }
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
        let mut builder = NewAuditEntry::builder();
        builder
            .subject(subject.into())
            .interaction_type(InteractionType::ApiCall)
            .action(action)
            .metadata(metadata)
            .outcome(outcome);
        if let Some(ms) = duration_ms {
            builder.duration_ms(ms);
        }
        let new_entry = builder.build().expect("all required fields set");
        Ok(self.repo.create(new_entry).await?)
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
    ) -> Result<AuditEntry, AuditError> {
        let mut builder = NewAuditEntry::builder();
        builder
            .subject(subject.into())
            .interaction_type(InteractionType::McpCall)
            .action(tool_name)
            .metadata(serde_json::json!({
                "tool_name": tool_name,
                "arguments": arguments,
            }))
            .outcome(outcome);
        if let Some(ms) = duration_ms {
            builder.duration_ms(ms);
        }
        let new_entry = builder.build().expect("all required fields set");
        Ok(self.repo.create(new_entry).await?)
    }

    /// Find audit entries by subject.
    #[instrument(name = "audit.find_by_subject", skip_all)]
    pub async fn find_by_subject(
        &self,
        subject: &str,
        limit: usize,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        let query = es_entity::PaginatedQueryArgs {
            first: limit,
            after: None,
        };
        let result = self
            .repo
            .list_for_subject_by_created_at(
                subject.to_string(),
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }
}
