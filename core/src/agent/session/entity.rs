use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::agent::ResetTimeDeltaSeconds;
use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;
use llm::prompt::{SystemBlock, Tool};

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
        model: String,
        system: Vec<SystemBlock>,
        tools: Vec<Tool>,
        max_tokens: u32,
        #[serde(default)]
        reset_time_delta_seconds: Option<ResetTimeDeltaSeconds>,
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
    pub model: String,
    pub system: Vec<SystemBlock>,
    pub tools: Vec<Tool>,
    pub max_tokens: u32,
    #[builder(default)]
    pub reset_time_delta_seconds: Option<ResetTimeDeltaSeconds>,
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
        self.start_new_thread(ThreadStartReason::InitialThread);
        Idempotent::Executed(())
    }

    /// Push a new thread carrying the session's current model/system/tools/
    /// max_tokens, emit the `ThreadStarted` event, and switch
    /// `current_thread` to it. Used both for the initial thread and for
    /// time-delta-driven resets.
    fn start_new_thread(&mut self, start_reason: ThreadStartReason) {
        let thread_id = SessionThreadId::new();
        let new_thread = NewSessionThread::builder()
            .id(thread_id)
            .session_id(self.id)
            .start_reason(start_reason)
            .model(self.model.clone())
            .system(self.system.clone())
            .tools(self.tools.clone())
            .max_tokens(self.max_tokens)
            .build()
            .expect("NewSessionThread build");
        self.threads.add_new(new_thread);

        self.events.push(AgentSessionEvent::ThreadStarted {
            thread_id,
            start_reason,
        });
        self.current_thread = thread_id;
    }

    fn current_thread(&mut self) -> &mut SessionThread {
        self.threads
            .get_persisted_mut(&self.current_thread)
            .expect("current thread present in nested collection")
    }

    pub fn add_user_message(
        &mut self,
        now: DateTime<Utc>,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<Idempotent<llm::Prompt>, AgentSessionError> {
        // If `reset_time_delta_seconds` is configured AND the current thread
        // has already received at least one user message AND that message is
        // older than the threshold, retire the thread and start a fresh one
        // before appending. The "first-message" check (no `last_at`) skips
        // the case where the current thread was just created and has
        // nothing but `Initialized`.
        if let Some(delta) = self.reset_time_delta_seconds {
            if let Some(last_at) = self.current_thread().last_user_message_at() {
                if delta.should_reset(last_at, now) {
                    let previous_thread = self.current_thread;
                    self.start_new_thread(ThreadStartReason::TimeDeltaExceeded {
                        previous_thread,
                    });
                }
            }
        }
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
                AgentSessionEvent::Initialized {
                    id,
                    agent_id,
                    model,
                    system,
                    tools,
                    max_tokens,
                    reset_time_delta_seconds,
                } => {
                    builder = builder
                        .id(*id)
                        .agent_id(*agent_id)
                        .model(model.clone())
                        .system(system.clone())
                        .tools(tools.clone())
                        .max_tokens(*max_tokens)
                        .reset_time_delta_seconds(*reset_time_delta_seconds);
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
    #[builder(setter(into))]
    pub(super) model: String,
    pub(super) system: Vec<SystemBlock>,
    pub(super) tools: Vec<Tool>,
    pub(super) max_tokens: u32,
    #[builder(default)]
    pub(super) reset_time_delta_seconds: Option<ResetTimeDeltaSeconds>,
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
                model: self.model,
                system: self.system,
                tools: self.tools,
                max_tokens: self.max_tokens,
                reset_time_delta_seconds: self.reset_time_delta_seconds,
            }],
        )
    }
}
