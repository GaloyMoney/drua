use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "McpCredsId")]
pub enum McpCredsEvent {
    Initialized {
        id: McpCredsId,
        owner: McpCredsOwner,
        name: String,
        token_hash: String,
        scopes: Vec<AuthScope>,
    },
    Revoked {
        revoked_at: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct McpCreds {
    pub id: McpCredsId,
    pub owner: McpCredsOwner,
    pub name: String,
    pub(crate) token_hash: String,
    pub scopes: Vec<AuthScope>,
    #[builder(setter(strip_option), default)]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    events: EntityEvents<McpCredsEvent>,
}

impl McpCreds {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn token_hash(&self) -> &str {
        &self.token_hash
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub(super) fn revoke(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: McpCredsEvent::Revoked { .. }
        );

        let revoked_at = chrono::Utc::now();
        self.events.push(McpCredsEvent::Revoked { revoked_at });
        self.revoked_at = Some(revoked_at);

        Idempotent::Executed(())
    }
}

impl core::fmt::Display for McpCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "McpCreds: {}, name: {}", self.id, self.name)
    }
}

impl TryFromEvents<McpCredsEvent> for McpCreds {
    fn try_from_events(events: EntityEvents<McpCredsEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = McpCredsBuilder::default();

        for event in events.iter_all() {
            match event {
                McpCredsEvent::Initialized {
                    id,
                    owner,
                    name,
                    token_hash,
                    scopes,
                } => {
                    builder = builder
                        .id(*id)
                        .owner(owner.clone())
                        .name(name.clone())
                        .token_hash(token_hash.clone())
                        .scopes(scopes.clone());
                }
                McpCredsEvent::Revoked { revoked_at } => {
                    builder = builder.revoked_at(*revoked_at);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct NewMcpCreds {
    #[builder(setter(into))]
    pub(super) id: McpCredsId,
    pub(super) owner: McpCredsOwner,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into))]
    pub(crate) token_hash: String,
    pub(super) scopes: Vec<AuthScope>,
}

impl NewMcpCreds {
    pub fn builder() -> NewMcpCredsBuilder {
        let mut builder = NewMcpCredsBuilder::default();
        builder = builder.id(McpCredsId::new());
        builder
    }
}

impl IntoEvents<McpCredsEvent> for NewMcpCreds {
    fn into_events(self) -> EntityEvents<McpCredsEvent> {
        EntityEvents::init(
            self.id,
            [McpCredsEvent::Initialized {
                id: self.id,
                owner: self.owner,
                name: self.name,
                token_hash: self.token_hash,
                scopes: self.scopes,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{Idempotent, IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{AuthScope, McpCredsId, McpCredsOwner, UserId};

    use super::{McpCreds, NewMcpCreds};

    fn new_mcp_creds() -> McpCreds {
        let new = NewMcpCreds::builder()
            .id(McpCredsId::new())
            .owner(McpCredsOwner::User {
                user_id: UserId::new(),
            })
            .name("test-creds")
            .token_hash("hash123")
            .scopes(vec![AuthScope::from("read"), AuthScope::from("write")])
            .build()
            .unwrap();

        McpCreds::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn mcp_creds_hydration() {
        let creds = new_mcp_creds();
        assert_eq!(creds.name, "test-creds");
        assert_eq!(
            creds.scopes,
            vec![AuthScope::from("read"), AuthScope::from("write")]
        );
        assert!(!creds.is_revoked());
    }

    #[test]
    fn mcp_creds_revoke() {
        let mut creds = new_mcp_creds();

        let result = creds.revoke();
        assert!(matches!(result, Idempotent::Executed(())));
        assert!(creds.is_revoked());

        let result = creds.revoke();
        assert!(matches!(result, Idempotent::AlreadyApplied));
    }
}
