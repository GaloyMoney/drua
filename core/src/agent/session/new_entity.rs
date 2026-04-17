use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::agent::ResetTimeDeltaSeconds;
use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;

use super::{
    error::AgentSessionError,
    message::*,
    metadata::*,
    new_thread::*,
    settings::*,
    AgentSessionId,
};

// ============================================================================
// Events
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStartReason {
    InitialThread,
}

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
    ThreadStarted {
        thread_id: SessionThreadId,
        start_reason: ThreadStartReason,
    },
    UserPromptAdded {
        thread_id: SessionThreadId,
        source: UserMessageSource,
        text: String,
    },
    AssistantResponseReceived {
        thread_id: SessionThreadId,
        content: Vec<AssistantBlock>,
        stop_reason: StopReason,
        error_message: Option<String>,
        metadata: AssistantResponseMetadata,
    },
    PromptSent {
        thread_id: SessionThreadId,
        // prompt <- leave blank for now
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

#[derive(Debug, Clone, Copy)]
pub enum TargetThread {
    Main,
}

#[derive(Debug, Clone)]
pub struct ToolUseRequest {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug)]
pub enum AgentSessionResponse {
    PromptPending,
    AwaitingAssistantResponse,
}

impl AgentSession {
    pub fn add_user_message(
        &mut self,
        target: TargetThread,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let thread_id = match target {
            TargetThread::Main => self.current_thread,
        };
        self.events.push(AgentSessionEvent::UserPromptAdded {
            thread_id,
            source,
            text: prompt,
        });
        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        if thread.is_user_turn() {
            Ok(AgentSessionResponse::PromptPending)
        } else {
            Ok(AgentSessionResponse::AwaitingAssistantResponse)
        }
    }

    pub fn next_prompt(&mut self, target: TargetThread) -> Result<Prompt, AgentSessionError> {
        let _thread_id = match target {
            TargetThread::Main => self.current_thread,
        };
        // TODO: lookup thread, collect pending user messages since last PromptSent
        unimplemented!()
    }

    pub fn init_initial_thread(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: AgentSessionEvent::ThreadStarted { .. }
        );
        let thread_id = SessionThreadId::new();
        let new_thread = NewSessionThread::builder()
            .id(thread_id)
            .session_id(self.id)
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
                AgentSessionEvent::ThreadStarted { thread_id, .. } => {
                    builder = builder.current_thread(*thread_id);
                }
                AgentSessionEvent::UserPromptAdded { .. } => {}
                AgentSessionEvent::AssistantResponseReceived { .. } => {}
                AgentSessionEvent::PromptSent { .. } => {}
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

#[cfg(test)]
mod tests {
    use crate::primitives::UserId;
    use es_entity::{Idempotent, IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn new_session() -> AgentSession {
        let new = NewAgentSession::builder()
            .agent_id(AgentId::new())
            .model_settings(ModelSettings {
                model: "test-model".into(),
                max_tokens: 1024,
            })
            .thread_simplification_settings(ThreadSimplificationSettings {
                simplify_after_idle_seconds: None,
            })
            .system_blocks(vec![])
            .tool_defs(vec![])
            .build()
            .expect("NewAgentSession build");
        AgentSession::try_from_events(new.into_events()).expect("hydrate")
    }

    /// Simulate a persist→reload cycle for nested threads.
    /// Drains "new" threads, round-trips them through events, and loads
    /// them into the "persisted" bucket so `get_persisted` can find them.
    fn hydrate_threads(session: &mut AgentSession) {
        let new_threads = session
            .threads
            .new_entities_mut()
            .drain(..)
            .map(|new| {
                SessionThread::try_from_events(new.into_events()).expect("hydrate thread")
            })
            .collect::<Vec<_>>();
        session.threads.load(new_threads);
    }

    fn user_source() -> UserMessageSource {
        UserMessageSource::User {
            user_id: UserId::new(),
        }
    }

    #[test]
    fn init_initial_thread_is_idempotent() {
        let mut session = new_session();

        let first = session.init_initial_thread();
        assert!(matches!(first, Idempotent::Executed(())));

        let second = session.init_initial_thread();
        assert!(matches!(second, Idempotent::AlreadyApplied));
    }

    #[test]
    fn add_user_message_returns_prompt_pending_on_user_turn() {
        let mut session = new_session();
        let _ = session.init_initial_thread();
        hydrate_threads(&mut session);

        let result = session.add_user_message(
            TargetThread::Main,
            user_source(),
            "Hello".into(),
        );
        assert!(
            matches!(result, Ok(AgentSessionResponse::PromptPending)),
            "expected PromptPending on fresh thread (user turn); got {result:?}"
        );
    }
}
