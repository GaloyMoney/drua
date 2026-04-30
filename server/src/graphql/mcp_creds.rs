use async_graphql::{InputObject, SimpleObject};

use super::primitives::*;

use drua_core::mcp_creds::McpCreds as DomainMcpCreds;

#[derive(SimpleObject, Clone)]
pub struct McpCreds {
    id: McpCredsId,
    name: String,
    revoked: bool,
    revoked_at: Option<Timestamp>,
    created_at: Timestamp,
}

impl From<DomainMcpCreds> for McpCreds {
    fn from(entity: DomainMcpCreds) -> Self {
        Self {
            id: entity.id,
            name: entity.name.clone(),
            revoked: entity.is_revoked(),
            revoked_at: entity.revoked_at.map(Timestamp::from),
            created_at: Timestamp::from(entity.created_at()),
        }
    }
}

#[derive(InputObject)]
pub struct McpCredentialsCreateInput {
    pub name: String,
    /// The scopes to grant, as strings (e.g. "admin", "project_admin:<id>").
    pub scopes: Vec<String>,
}

/// Returned on create — includes the plaintext token (shown only once).
#[derive(SimpleObject)]
pub struct McpCredentialsCreatePayload {
    pub mcp_creds: McpCreds,
    /// The plaintext token. Only returned once on creation.
    pub token: String,
}

#[derive(InputObject)]
pub struct McpCredentialsRevokeInput {
    pub id: McpCredsId,
}

mutation_payload! { McpCredentialsRevokePayload, mcp_creds: McpCreds }
