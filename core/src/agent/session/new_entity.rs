use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::agent::ResetTimeDeltaSeconds;
use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;

use super::{
    error::AgentSessionError,
    message::*,
    thread::{NewSessionThread, SessionThread, SessionThreadId, ThreadStartReason},
    AgentSessionId,
};

// ============================================================================
// Supporting types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSettings {
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSimplificationSettings {
    pub simplify_after_idle_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantResponseMetadata {
    pub api: String,
    pub model: String,
    pub usage: Usage,
    pub cost: Cost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

// ============================================================================
// Events
// ============================================================================

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentSessionId")]
pub enum AgentSessionEvent {
    Initialized {
        id: AgentSessionId,
        agent_id: AgentId,
        model_settings: ModelSettings,
        thread_simplification_settings: ThreadSimplificationSettings,
        system_blocks: Vec<SystemBlock>,
        tool_defs: Vec<ToolDefinition>,
    },
    UserBlocksAdded {
        blocks: Vec<UserBlock>,
    },
    AssistantResponseReceived {
        content: Vec<AssistantBlock>,
        stop_reason: StopReason,
        error_message: Option<String>,
        metadata: AssistantResponseMetadata,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub agent_id: AgentId,
    // #[builder(default = "SessionThreadId::from(uuid::Uuid::nil())")]
    // current_thread: SessionThreadId,
    events: EntityEvents<AgentSessionEvent>,
    // #[es_entity(nested)]
    // #[builder(default)]
    // pub(super) threads: Nested<SessionThread>,
}

impl TryFromEvents<AgentSessionEvent> for AgentSession {
    fn try_from_events(
        events: EntityEvents<AgentSessionEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = AgentSessionBuilder::default();

        for event in events.iter_all() {
            match event {
                AgentSessionEvent::Initialized { id, agent_id, .. } => {
                    builder = builder.id(*id).agent_id(*agent_id);
                }
                AgentSessionEvent::UserBlocksAdded { .. } => {}
                AgentSessionEvent::AssistantResponseReceived { .. } => {}
            }
        }

        builder.events(events).build()
    }
}

// impl AgentSession {
//     /// Push the initial thread. Idempotent — if a thread has already been
//     /// started this is a no-op.
//     pub fn init_initial_thread(&mut self) -> Idempotent<()> {
//         idempotency_guard!(
//             self.events.iter_all().rev(),
//             already_applied: AgentSessionEvent::ThreadStarted { .. }
//         );
//         self.start_new_thread(ThreadStartReason::InitialThread);
//         Idempotent::Executed(())
//     }

//     /// Push a new thread carrying the session's current model/system/tools/
//     /// max_tokens, emit the `ThreadStarted` event, and switch
//     /// `current_thread` to it. Used both for the initial thread and for
//     /// time-delta-driven resets.
//     fn start_new_thread(&mut self, start_reason: ThreadStartReason) {
//         let thread_id = SessionThreadId::new();
//         let new_thread = NewSessionThread::builder()
//             .id(thread_id)
//             .session_id(self.id)
//             .start_reason(start_reason)
//             .model(self.model.clone())
//             .system(self.system.clone())
//             .tools(self.tools.clone())
//             .max_tokens(self.max_tokens)
//             .build()
//             .expect("NewSessionThread build");
//         self.threads.add_new(new_thread);

//         self.events.push(AgentSessionEvent::ThreadStarted {
//             thread_id,
//             start_reason,
//         });
//         self.current_thread = thread_id;
//     }

//     fn current_thread(&mut self) -> &mut SessionThread {
//         self.threads
//             .get_persisted_mut(&self.current_thread)
//             .expect("current thread present in nested collection")
//     }

//     pub fn add_user_message(
//         &mut self,
//         now: DateTime<Utc>,
//         source: UserMessageSource,
//         prompt: String,
//     ) -> Result<Idempotent<llm::Prompt>, AgentSessionError> {
//         self.current_thread().add_user_message(source, prompt)
//     }

//     pub fn add_prompt_response(
//         &mut self,
//         response: llm::PromptResponse,
//     ) -> Vec<llm::RequestToolUse> {
//         self.current_thread().add_prompt_response(response)
//     }

//     pub fn add_tool_results(&mut self, results: Vec<llm::ToolUseResult>) -> llm::Prompt {
//         self.current_thread().add_tool_results(results)
//     }
// }

// impl TryFromEvents<AgentSessionEvent> for AgentSession {
//     fn try_from_events(
//         events: EntityEvents<AgentSessionEvent>,
//     ) -> Result<Self, EntityHydrationError> {
//         let mut builder = AgentSessionBuilder::default();

//         for event in events.iter_all() {
//             match event {
//                 AgentSessionEvent::Initialized {
//                     id,
//                     agent_id,
//                     model,
//                     system,
//                     tools,
//                     max_tokens,
//                 } => {
//                     builder = builder
//                         .id(*id)
//                         .agent_id(*agent_id)
//                         .model(model.clone())
//                         .system(system.clone())
//                         .tools(tools.clone())
//                         .max_tokens(*max_tokens)
//                 }
//                 AgentSessionEvent::ThreadStarted { thread_id, .. } => {
//                     builder = builder.current_thread(*thread_id);
//                 }
//             }
//         }

//         builder.events(events).build()
//     }
// }

#[derive(Debug, Builder)]
pub struct NewAgentSession {
    #[builder(setter(into))]
    pub(super) id: AgentSessionId,
    pub(super) agent_id: AgentId,
    pub(super) model_settings: ModelSettings,
    pub(super) thread_simplification_settings: ThreadSimplificationSettings,
    pub(super) system_blocks: Vec<SystemBlock>,
    pub(super) tool_defs: Vec<ToolDefinition>,
}

impl NewAgentSession {
    pub fn builder() -> NewAgentSessionBuilder {
        let mut builder = NewAgentSessionBuilder::default();
        builder.id(AgentSessionId::new());
        builder
    }
}

impl IntoEvents<AgentSessionEvent> for NewAgentSession {
    fn into_events(self) -> EntityEvents<AgentSessionEvent> {
        EntityEvents::init(
            self.id,
            [AgentSessionEvent::Initialized {
                id: self.id,
                agent_id: self.agent_id,
                model_settings: self.model_settings,
                thread_simplification_settings: self.thread_simplification_settings,
                system_blocks: self.system_blocks,
                tool_defs: self.tool_defs,
            }],
        )
    }
}
