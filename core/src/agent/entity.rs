use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::{AgentId, AuthSubject, UserId, WorkspaceId};
use es_entity::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum AgentRole {
    WorkspaceLead,
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentId")]
pub enum AgentEvent {
    Initialized {
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: String,
        authz_scopes: Vec<String>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Agent {
    pub id: AgentId,
    pub workspace_id: WorkspaceId,
    pub agent_role: AgentRole,
    pub name: String,
    pub authz_scopes: Vec<String>,
    events: EntityEvents<AgentEvent>,
}

impl Agent {
    /// The auth subject this agent acts as when invoking tools — its own
    /// workspace + id, carrying the scopes persisted on the `Initialized`
    /// event. Use when no originating user can be attributed.
    pub fn auth_subject(&self) -> AuthSubject {
        AuthSubject::Agent(self.workspace_id, self.id, self.authz_scopes.clone())
    }

    /// Same as [`Self::auth_subject`] but tagged with the user that triggered
    /// the agent's work, so downstream actions can be attributed back to them.
    pub fn auth_subject_for_user(&self, user_id: UserId) -> AuthSubject {
        AuthSubject::AgentOnBehalfOfUser(
            user_id,
            self.workspace_id,
            self.id,
            self.authz_scopes.clone(),
        )
    }
}

impl TryFromEvents<AgentEvent> for Agent {
    fn try_from_events(events: EntityEvents<AgentEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = AgentBuilder::default();

        for event in events.iter_all() {
            match event {
                AgentEvent::Initialized {
                    id,
                    workspace_id,
                    agent_role,
                    name,
                    authz_scopes,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_role(*agent_role)
                        .name(name.clone())
                        .authz_scopes(authz_scopes.clone());
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
    pub(super) workspace_id: WorkspaceId,
    pub(super) agent_role: AgentRole,
    #[builder(setter(into))]
    pub(super) name: String,
    pub(super) authz_scopes: Vec<String>,
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
                workspace_id: self.workspace_id,
                agent_role: self.agent_role,
                name: self.name,
                authz_scopes: self.authz_scopes,
            }],
        )
    }
}
