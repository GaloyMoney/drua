use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

/// Configuration for an agent's sandbox environment (infrastructure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Enable persistent volume for the sandbox workspace.
    #[serde(default)]
    pub persistent_volume: bool,
    /// PVC size (e.g., "10Gi"). Only used when persistent_volume is true.
    #[serde(default = "SandboxConfig::default_pvc_size")]
    pub pvc_size: String,
    /// CPU resource request/limit (e.g., "500m", "1").
    #[serde(default = "SandboxConfig::default_resource_cpu")]
    pub resource_cpu: String,
    /// Memory resource request/limit (e.g., "512Mi", "2Gi").
    #[serde(default = "SandboxConfig::default_resource_mem")]
    pub resource_mem: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            persistent_volume: false,
            pvc_size: Self::default_pvc_size(),
            resource_cpu: Self::default_resource_cpu(),
            resource_mem: Self::default_resource_mem(),
        }
    }
}

impl SandboxConfig {
    fn default_pvc_size() -> String {
        "10Gi".to_string()
    }

    fn default_resource_cpu() -> String {
        "500m".to_string()
    }

    fn default_resource_mem() -> String {
        "512Mi".to_string()
    }
}

/// Configuration for agent chat behavior (LLM interaction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConfig {
    /// LLM model (e.g., "claude-sonnet-4-6").
    #[serde(default = "ChatConfig::default_model")]
    pub model: String,
    /// Max tokens per API response.
    #[serde(default = "ChatConfig::default_max_tokens")]
    pub max_tokens: u32,
    /// Max turns per conversation exchange.
    #[serde(default = "ChatConfig::default_max_turns")]
    pub max_turns: u32,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            model: Self::default_model(),
            max_tokens: Self::default_max_tokens(),
            max_turns: Self::default_max_turns(),
        }
    }
}

impl ChatConfig {
    fn default_model() -> String {
        "claude-sonnet-4-20250514".to_string()
    }

    fn default_max_tokens() -> u32 {
        4096
    }

    fn default_max_turns() -> u32 {
        25
    }
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
        mcp_creds_id: McpCredsId,
        sandbox_config: SandboxConfig,
        chat_config: ChatConfig,
    },
    SandboxProvisioned {},
    SandboxReady {},
    SandboxLost {},
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
    pub mcp_creds_id: McpCredsId,
    pub sandbox_config: SandboxConfig,
    pub chat_config: ChatConfig,
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

    pub(super) fn sandbox_provisioned(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentEvent::SandboxProvisioned { .. },
            resets_on: AgentEvent::SandboxLost { .. }
        );

        self.sandbox_state = SandboxState::Provisioning;
        self.events.push(AgentEvent::SandboxProvisioned {});
        Idempotent::Executed(())
    }

    pub(super) fn sandbox_ready(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentEvent::SandboxReady { .. },
            resets_on: AgentEvent::SandboxLost { .. }
        );

        self.sandbox_state = SandboxState::Ready;
        self.events.push(AgentEvent::SandboxReady {});
        Idempotent::Executed(())
    }

    pub(super) fn sandbox_lost(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentEvent::SandboxLost { .. },
            resets_on: AgentEvent::SandboxProvisioned { .. }
        );

        self.sandbox_state = SandboxState::None;
        self.events.push(AgentEvent::SandboxLost {});
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
                    workspace_id,
                    agent_type,
                    name,
                    mcp_creds_id,
                    sandbox_config,
                    chat_config,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_type(*agent_type)
                        .name(name.clone())
                        .mcp_creds_id(*mcp_creds_id)
                        .sandbox_config(sandbox_config.clone())
                        .chat_config(chat_config.clone())
                        .sandbox_state(SandboxState::None);
                }
                AgentEvent::SandboxProvisioned {} => {
                    builder = builder.sandbox_state(SandboxState::Provisioning);
                }
                AgentEvent::SandboxReady {} => {
                    builder = builder.sandbox_state(SandboxState::Ready);
                }
                AgentEvent::SandboxLost {} => {
                    builder = builder.sandbox_state(SandboxState::None);
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
    pub(super) mcp_creds_id: McpCredsId,
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
                mcp_creds_id: self.mcp_creds_id,
                sandbox_config: self.sandbox_config,
                chat_config: self.chat_config,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{AgentId, AgentType, McpCredsId, WorkspaceId};

    use super::{Agent, ChatConfig, NewAgent, SandboxConfig, SandboxState};

    fn new_agent() -> Agent {
        let new = NewAgent::builder()
            .id(AgentId::new())
            .workspace_id(WorkspaceId::new())
            .agent_type(AgentType::WorkspaceLead)
            .name("workspace-lead")
            .mcp_creds_id(McpCredsId::new())
            .sandbox_config(SandboxConfig {
                persistent_volume: true,
                ..Default::default()
            })
            .chat_config(ChatConfig {
                model: "claude-sonnet-4-6".to_string(),
                max_turns: 10,
                ..Default::default()
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
        assert_eq!(agent.chat_config.model, "claude-sonnet-4-6");
        assert_eq!(agent.chat_config.max_turns, 10);
    }
}
