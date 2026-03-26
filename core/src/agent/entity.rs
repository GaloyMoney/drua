use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentId")]
pub enum AgentEvent {
    Initialized {
        id: AgentId,
        user_id: UserId,
        name: String,
        token_hash: String,
        scopes: Vec<String>,
    },
    Revoked {
        revoked_at: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Agent {
    pub id: AgentId,
    pub user_id: UserId,
    pub name: String,
    pub(crate) token_hash: String,
    pub scopes: Vec<String>,
    #[builder(setter(strip_option), default)]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    events: EntityEvents<AgentEvent>,
}

impl Agent {
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
            already_applied: AgentEvent::Revoked { .. }
        );

        let revoked_at = chrono::Utc::now();
        self.events.push(AgentEvent::Revoked { revoked_at });
        self.revoked_at = Some(revoked_at);

        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Agent: {}, name: {}", self.id, self.name)
    }
}

impl TryFromEvents<AgentEvent> for Agent {
    fn try_from_events(events: EntityEvents<AgentEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = AgentBuilder::default();

        for event in events.iter_all() {
            match event {
                AgentEvent::Initialized {
                    id,
                    user_id,
                    name,
                    token_hash,
                    scopes,
                } => {
                    builder = builder
                        .id(*id)
                        .user_id(*user_id)
                        .name(name.clone())
                        .token_hash(token_hash.clone())
                        .scopes(scopes.clone());
                }
                AgentEvent::Revoked { revoked_at } => {
                    builder = builder.revoked_at(*revoked_at);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewAgent {
    #[builder(setter(into))]
    pub(super) id: AgentId,
    pub(super) user_id: UserId,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into))]
    pub(crate) token_hash: String,
    pub(super) scopes: Vec<String>,
}

impl NewAgent {
    pub fn builder() -> NewAgentBuilder {
        let mut builder = NewAgentBuilder::default();
        builder.id(AgentId::new());
        builder
    }
}

impl IntoEvents<AgentEvent> for NewAgent {
    fn into_events(self) -> EntityEvents<AgentEvent> {
        EntityEvents::init(
            self.id,
            [AgentEvent::Initialized {
                id: self.id,
                user_id: self.user_id,
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

    use crate::primitives::{AgentId, UserId};

    use super::{Agent, NewAgent};

    fn new_agent() -> Agent {
        let new_agent = NewAgent::builder()
            .id(AgentId::new())
            .user_id(UserId::new())
            .name("test-agent")
            .token_hash("hash123")
            .scopes(vec!["read".to_string(), "write".to_string()])
            .build()
            .unwrap();

        Agent::try_from_events(new_agent.into_events()).unwrap()
    }

    #[test]
    fn agent_hydration() {
        let agent = new_agent();
        assert_eq!(agent.name, "test-agent");
        assert_eq!(agent.scopes, vec!["read", "write"]);
        assert!(!agent.is_revoked());
    }

    #[test]
    fn agent_revoke() {
        let mut agent = new_agent();

        let result = agent.revoke();
        assert!(matches!(result, Idempotent::Executed(())));
        assert!(agent.is_revoked());

        let result = agent.revoke();
        assert!(matches!(result, Idempotent::AlreadyApplied));
    }
}
