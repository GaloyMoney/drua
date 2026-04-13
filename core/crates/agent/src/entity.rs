use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;
use primitives::{AgentId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum AgentRole {
    WorkspaceLead,
}

/// Event emitted while the agent is processing a message.
/// Streamed back to callers via an mpsc channel.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageEvent {
    UserMessage {
        source: primitives::UserMessageSource,
        text: String,
    },
    AssistantText {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
    },
    ToolResult {
        name: String,
        is_error: bool,
    },
    Done {
        turns: u32,
        input_tokens: u32,
        output_tokens: u32,
    },
    Error {
        message: String,
    },
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
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Agent {
    pub id: AgentId,
    pub workspace_id: WorkspaceId,
    pub agent_role: AgentRole,
    pub name: String,
    events: EntityEvents<AgentEvent>,
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
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_role(*agent_role)
                        .name(name.clone());
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
            }],
        )
    }
}
