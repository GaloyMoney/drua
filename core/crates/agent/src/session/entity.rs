use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;
use primitives::{AgentId, UserMessageSource};

use super::{
    error::AgentSessionError,
    thread::{NewSessionThread, SessionThread, SessionThreadId, ThreadStartReason},
    AgentSessionId,
};

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentSessionId")]
pub enum AgentSessionEvent {
    Initialized {
        id: AgentSessionId,
        agent_id: AgentId,
    },
    ThreadStarted {
        thread_id: SessionThreadId,
        start_reason: ThreadStartReason,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub agent_id: AgentId,
    #[builder(default = "SessionThreadId::from(uuid::Uuid::nil())")]
    current_thread: SessionThreadId,
    events: EntityEvents<AgentSessionEvent>,

    #[es_entity(nested)]
    #[builder(default)]
    pub(super) threads: Nested<SessionThread>,
}

impl AgentSession {
    /// Push the initial thread. Idempotent — if a thread has already been
    /// started this is a no-op.
    pub fn init_initial_thread(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentSessionEvent::ThreadStarted { .. }
        );

        let thread_id = SessionThreadId::new();
        let new_thread = NewSessionThread::builder()
            .id(thread_id)
            .session_id(self.id)
            .start_reason(ThreadStartReason::InitialThread)
            .build()
            .expect("NewSessionThread build");
        self.threads.add_new(new_thread);

        self.events.push(AgentSessionEvent::ThreadStarted {
            thread_id,
            start_reason: ThreadStartReason::InitialThread,
        });
        self.current_thread = thread_id;

        Idempotent::Executed(())
    }

    fn current_thread(&mut self) -> &mut SessionThread {
        self.threads
            .get_persisted_mut(&self.current_thread)
            .expect("current thread present in nested collection")
    }

    pub fn add_user_message(
        &mut self,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<Idempotent<llm::Prompt>, AgentSessionError> {
        self.current_thread().add_user_message(source, prompt)
    }

    pub fn add_prompt_response(
        &mut self,
        response: llm::PromptResponse,
    ) -> Vec<llm::RequestToolUse> {
        self.current_thread().add_prompt_response(response)
    }

    pub fn add_tool_results(&mut self, results: Vec<llm::ToolUseResult>) -> llm::Prompt {
        self.current_thread().add_tool_results(results)
    }
}

impl TryFromEvents<AgentSessionEvent> for AgentSession {
    fn try_from_events(
        events: EntityEvents<AgentSessionEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = AgentSessionBuilder::default();

        for event in events.iter_all() {
            match event {
                AgentSessionEvent::Initialized { id, agent_id } => {
                    builder = builder.id(*id).agent_id(*agent_id);
                }
                AgentSessionEvent::ThreadStarted { thread_id, .. } => {
                    builder = builder.current_thread(*thread_id);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewAgentSession {
    #[builder(setter(into))]
    pub(super) id: AgentSessionId,
    pub(super) agent_id: AgentId,
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
            }],
        )
    }
}
