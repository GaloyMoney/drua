use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

/// Configuration for an agent's sandbox environment (infrastructure).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// PVC size (e.g., "10Gi").
    #[serde(default = "SandboxConfig::default_pvc_size")]
    pub pvc_size: String,
    /// CPU resource request/limit (e.g., "500m", "1").
    #[serde(default = "SandboxConfig::default_resource_cpu")]
    pub resource_cpu: String,
    /// Memory resource request/limit (e.g., "512Mi", "2Gi").
    #[serde(default = "SandboxConfig::default_resource_mem")]
    pub resource_mem: String,
    /// Built-in tools to disallow in the sandbox (e.g., ["Bash", "Edit"]).
    /// When empty, all tools are available.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            pvc_size: Self::default_pvc_size(),
            resource_cpu: Self::default_resource_cpu(),
            resource_mem: Self::default_resource_mem(),
            disallowed_tools: Vec::new(),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        "claude-haiku-4-5-20251001".to_string()
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
#[allow(clippy::large_enum_variant)]
pub enum AgentEvent {
    Initialized {
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        name: String,
        sandbox_config: SandboxConfig,
        chat_config: ChatConfig,
    },
    ChatConfigUpdated {
        chat_config: ChatConfig,
    },
    SandboxConfigUpdated {
        sandbox_config: SandboxConfig,
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

    pub(super) fn update_chat_config(&mut self, new_config: ChatConfig) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentEvent::ChatConfigUpdated { chat_config }
                if *chat_config == new_config,
            resets_on: AgentEvent::ChatConfigUpdated { .. }
        );

        self.chat_config = new_config.clone();
        self.events.push(AgentEvent::ChatConfigUpdated {
            chat_config: new_config,
        });
        Idempotent::Executed(())
    }

    pub(super) fn update_sandbox_config(&mut self, new_config: SandboxConfig) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentEvent::SandboxConfigUpdated { sandbox_config }
                if *sandbox_config == new_config,
            resets_on: AgentEvent::SandboxConfigUpdated { .. }
        );

        self.sandbox_config = new_config.clone();
        self.events.push(AgentEvent::SandboxConfigUpdated {
            sandbox_config: new_config,
        });
        Idempotent::Executed(())
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
                    sandbox_config,
                    chat_config,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_type(*agent_type)
                        .name(name.clone())
                        .sandbox_config(sandbox_config.clone())
                        .chat_config(chat_config.clone())
                        .sandbox_state(SandboxState::None);
                }
                AgentEvent::ChatConfigUpdated { chat_config } => {
                    builder = builder.chat_config(chat_config.clone());
                }
                AgentEvent::SandboxConfigUpdated { sandbox_config } => {
                    builder = builder.sandbox_config(sandbox_config.clone());
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
            .sandbox_config(SandboxConfig::default())
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
        assert_eq!(agent.chat_config.model, "claude-sonnet-4-6");
        assert_eq!(agent.chat_config.max_turns, 10);
    }

    #[test]
    fn agent_update_chat_config() {
        let mut agent = new_agent();
        let new_config = ChatConfig {
            model: "claude-opus-4-6".to_string(),
            max_tokens: 8192,
            max_turns: 50,
        };
        assert!(agent.update_chat_config(new_config.clone()).did_execute());
        assert_eq!(agent.chat_config.model, "claude-opus-4-6");
        assert_eq!(agent.chat_config.max_tokens, 8192);
        assert_eq!(agent.chat_config.max_turns, 50);

        // Idempotent: same config again should be a no-op
        assert!(!agent.update_chat_config(new_config).did_execute());
    }

    #[test]
    fn agent_update_sandbox_config() {
        let mut agent = new_agent();
        let new_config = SandboxConfig {
            resource_cpu: "2".to_string(),
            resource_mem: "2Gi".to_string(),
            ..Default::default()
        };
        assert!(agent
            .update_sandbox_config(new_config.clone())
            .did_execute());
        assert_eq!(agent.sandbox_config.resource_cpu, "2");
        assert_eq!(agent.sandbox_config.resource_mem, "2Gi");

        // Idempotent: same config again should be a no-op
        assert!(!agent.update_sandbox_config(new_config).did_execute());
    }
}
