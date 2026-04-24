use async_graphql::{InputObject, SimpleObject};

use super::primitives::*;

use drua_core::audit::primitives::AuditEntry as DomainAuditEntry;

#[derive(SimpleObject, Clone)]
pub struct AuditEntry {
    acting_user_id: Option<UUID>,
    acting_agent_id: Option<UUID>,
    on_behalf_of_user_id: Option<UUID>,
    entrypoint: Option<String>,
    interaction_type: String,
    action: String,
    outcome: String,
    error: Option<bool>,
    duration_ms: Option<i32>,
    tokens_returned: Option<i32>,
    recorded_at: Timestamp,
}

impl From<DomainAuditEntry> for AuditEntry {
    fn from(e: DomainAuditEntry) -> Self {
        Self {
            acting_user_id: e.acting_user_id.map(|id| UUID::from(uuid::Uuid::from(id))),
            acting_agent_id: e.acting_agent_id.map(|id| UUID::from(uuid::Uuid::from(id))),
            on_behalf_of_user_id: e
                .on_behalf_of_user_id
                .map(|id| UUID::from(uuid::Uuid::from(id))),
            entrypoint: e.entrypoint,
            interaction_type: e.interaction_type,
            action: e.action,
            outcome: e.outcome,
            error: e.error,
            duration_ms: e.duration_ms.map(|v| v as i32),
            tokens_returned: e.tokens_returned.map(|v| v as i32),
            recorded_at: Timestamp::from(e.recorded_at),
        }
    }
}

#[derive(InputObject)]
pub struct AuditLogQueryInput {
    pub workspace_id: Option<WorkspaceId>,
    pub acting_agent_id: Option<AgentId>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub error: Option<bool>,
    #[graphql(default = 20)]
    pub limit: i32,
}
