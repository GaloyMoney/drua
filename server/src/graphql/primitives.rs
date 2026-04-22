use serde::{Deserialize, Serialize};

// Re-exported for use by per-entity GraphQL type modules (workspace.rs,
// agent.rs, etc.) as they are built out in subsequent PRs.
#[allow(unused_imports)]
pub use drua_core::primitives::{
    AgentId, McpCredsId, SandboxId, SkillId, UserId, WorkspaceId, WorkspaceSecretId,
};

#[allow(unused_imports)]
pub use drua_core::agent::session::SessionThreadId;

#[allow(unused_imports)]
pub use drua_core::auth::AuthSubject;

pub use es_entity::graphql::UUID;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Yaml(String);
async_graphql::scalar!(Yaml);

impl From<String> for Yaml {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(chrono::DateTime<chrono::Utc>);
async_graphql::scalar!(
    Timestamp,
    "Timestamp",
    "An ISO 8601 UTC timestamp (e.g., 2024-01-15T09:30:00Z). Always in UTC."
);

impl From<chrono::DateTime<chrono::Utc>> for Timestamp {
    fn from(value: chrono::DateTime<chrono::Utc>) -> Self {
        Self(value)
    }
}

impl Timestamp {
    #[allow(dead_code)]
    pub fn into_inner(self) -> chrono::DateTime<chrono::Utc> {
        self.0
    }
}

#[derive(async_graphql::Enum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDirection {
    Asc,
    #[default]
    Desc,
}

impl From<SortDirection> for es_entity::ListDirection {
    fn from(direction: SortDirection) -> Self {
        match direction {
            SortDirection::Asc => Self::Ascending,
            SortDirection::Desc => Self::Descending,
        }
    }
}
