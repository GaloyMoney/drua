use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

/// Configuration for an agent's sandbox environment (infrastructure).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Enable persistent volume for the sandbox workspace.
    #[serde(default)]
    pub persistent_volume: bool,
    /// PVC size (e.g., "10Gi"). Only used when persistent_volume is true.
    #[serde(default = "default_pvc_size")]
    pub pvc_size: String,
    /// CPU resource request/limit (e.g., "500m", "1").
    #[serde(default)]
    pub resource_cpu: Option<String>,
    /// Memory resource request/limit (e.g., "512Mi", "2Gi").
    #[serde(default)]
    pub resource_mem: Option<String>,
}

fn default_pvc_size() -> String {
    "10Gi".to_string()
}

/// Configuration for agent chat behavior (LLM interaction).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatConfig {
    /// Default LLM model for the agent harness (e.g., "claude-sonnet-4-6").
    #[serde(default)]
    pub model: Option<String>,
    /// Default max turns per conversation exchange.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentId")]
pub enum AgentEvent {
    Initialized {
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        name: String,
        #[serde(default)]
        sandbox_config: SandboxConfig,
        #[serde(default)]
        chat_config: ChatConfig,
    },
    SandboxProvisioned {},
    SandboxReady {},
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
    pub sandbox_config: SandboxConfig,
    #[builder(default)]
    pub chat_config: ChatConfig,
    #[builder(default)]
    pub sandbox_state: SandboxState,
    events: EntityEvents<AgentEvent>,
}

impl Agent {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    /// Deterministic sandbox name derived from the agent ID.
    pub fn sandbox_name(&self) -> String {
        format!("agent-{}", &self.id.to_string()[..8])
    }

    pub(super) fn sandbox_provisioned(&mut self) {
        self.sandbox_state = SandboxState::Provisioning;
        self.events.push(AgentEvent::SandboxProvisioned {});
    }

    pub(super) fn sandbox_ready(&mut self) {
        self.sandbox_state = SandboxState::Ready;
        self.events.push(AgentEvent::SandboxReady {});
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
                    sandbox_config,
                    chat_config,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_type(*agent_type)
                        .name(name.clone())
                        .sandbox_config(sandbox_config.clone())
                        .chat_config(chat_config.clone());
                }
                AgentEvent::SandboxProvisioned {} => {
                    builder = builder.sandbox_state(SandboxState::Provisioning);
                }
                AgentEvent::SandboxReady {} => {
                    builder = builder.sandbox_state(SandboxState::Ready);
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
    #[builder(default)]
    pub(super) sandbox_config: SandboxConfig,
    #[builder(default)]
    pub(super) chat_config: ChatConfig,
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
                sandbox_config: self.sandbox_config,
                chat_config: self.chat_config,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{AgentId, AgentType, WorkspaceId};

    use super::{Agent, ChatConfig, NewAgent, SandboxConfig, SandboxState};

    fn new_agent() -> Agent {
        let new = NewAgent::builder()
            .id(AgentId::new())
            .workspace_id(WorkspaceId::new())
            .agent_type(AgentType::WorkspaceLead)
            .name("workspace-lead")
            .sandbox_config(SandboxConfig {
                persistent_volume: true,
                ..Default::default()
            })
            .chat_config(ChatConfig {
                model: Some("claude-sonnet-4-6".to_string()),
                max_turns: Some(10),
            })
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
        assert!(agent.sandbox_name().starts_with("agent-"));
        assert!(agent.sandbox_config.persistent_volume);
        assert_eq!(
            agent.chat_config.model,
            Some("claude-sonnet-4-6".to_string())
        );
        assert_eq!(agent.chat_config.max_turns, Some(10));
    }
}
