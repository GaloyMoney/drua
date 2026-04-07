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
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        name: String,
    },
    SandboxProvisioned {
        sandbox_name: String,
    },
    SandboxReady {
        sandbox_name: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    #[default]
    None,
    Provisioning,
    Ready,
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Agent {
    pub id: AgentId,
    pub workspace_id: WorkspaceId,
    pub agent_type: AgentType,
    pub name: String,
    #[builder(default)]
    pub sandbox_state: SandboxState,
    #[builder(setter(strip_option), default)]
    pub sandbox_name: Option<String>,
    events: EntityEvents<AgentEvent>,
}

impl Agent {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub(super) fn sandbox_provisioned(&mut self, sandbox_name: String) {
        self.sandbox_state = SandboxState::Provisioning;
        self.sandbox_name = Some(sandbox_name.clone());
        self.events
            .push(AgentEvent::SandboxProvisioned { sandbox_name });
    }

    pub(super) fn sandbox_ready(&mut self, sandbox_name: String) {
        self.sandbox_state = SandboxState::Ready;
        self.sandbox_name = Some(sandbox_name.clone());
        self.events.push(AgentEvent::SandboxReady { sandbox_name });
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
                    workspace_id,
                    agent_type,
                    name,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_type(*agent_type)
                        .name(name.clone());
                }
                AgentEvent::SandboxProvisioned { sandbox_name } => {
                    builder = builder
                        .sandbox_state(SandboxState::Provisioning)
                        .sandbox_name(sandbox_name.clone());
                }
                AgentEvent::SandboxReady { sandbox_name } => {
                    builder = builder
                        .sandbox_state(SandboxState::Ready)
                        .sandbox_name(sandbox_name.clone());
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
    pub(super) agent_type: AgentType,
    #[builder(setter(into))]
    pub(super) name: String,
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
                agent_type: self.agent_type,
                name: self.name,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{AgentId, AgentType, WorkspaceId};

    use super::{Agent, NewAgent, SandboxState};

    fn new_agent() -> Agent {
        let new = NewAgent::builder()
            .id(AgentId::new())
            .workspace_id(WorkspaceId::new())
            .agent_type(AgentType::WorkspaceLead)
            .name("workspace-lead")
            .build()
            .unwrap();

        Agent::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn agent_hydration() {
        let agent = new_agent();
        assert_eq!(agent.name, "workspace-lead");
        assert_eq!(agent.agent_type, AgentType::WorkspaceLead);
        assert_eq!(agent.sandbox_state, SandboxState::None);
        assert_eq!(agent.sandbox_name, None);
    }
}
