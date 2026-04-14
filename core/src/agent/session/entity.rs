use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::agent::ResetTimeDeltaSeconds;
use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;
use llm::prompt::{AssistantBlock, SystemBlock, Tool};

use super::{
    error::AgentSessionError,
    thread::{SessionThreadId, ThreadStartReason, ThreadView},
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
    UserMessage {
        source: UserMessageSource,
        text: String,
    },
    AssistantResponse {
        content: Vec<AssistantBlock>,
    },
    ToolResults {
        results: Vec<llm::ToolUseResult>,
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
    events: EntityEvents<AgentSessionEvent>,

    #[builder(default)]
    messages: Vec<llm::prompt::Message>,
    #[builder(default)]
    threads: Vec<ThreadView>,
    #[builder(default)]
    current_thread_idx: usize,
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

    fn start_new_thread(&mut self, start_reason: ThreadStartReason) {
        let thread_id = SessionThreadId::new();
        self.events.push(AgentSessionEvent::ThreadStarted {
            thread_id,
            start_reason,
        });
        self.threads.push(ThreadView {
            id: thread_id,
            start_reason,
            message_indices: Vec::new(),
        });
        self.current_thread_idx = self.threads.len() - 1;
    }

    fn build_prompt(&self) -> llm::Prompt {
        let thread = &self.threads[self.current_thread_idx];
        let messages: Vec<llm::prompt::Message> = thread
            .message_indices
            .iter()
            .map(|&idx| self.messages[idx].clone())
            .collect();

        llm::Prompt {
            model: self.model.clone(),
            system: self.system.clone(),
            tools: self.tools.clone(),
            messages,
            tool_choice: None,
            max_tokens: Some(self.max_tokens),
        }
    }

    /// Timestamp of the most recent persisted `UserMessage` event, if any.
    /// Returns `None` while the current thread has no user messages.
    fn last_user_message_at(&self) -> Option<DateTime<Utc>> {
        self.events
            .iter_persisted()
            .rev()
            .find(|e| matches!(e.event, AgentSessionEvent::UserMessage { .. }))
            .map(|e| e.recorded_at)
    }

    /// Returns the last message in the current thread (by looking up the
    /// message vec via the thread's index list), or `None` if the thread
    /// has no messages yet.
    fn current_thread_last_message(&self) -> Option<&llm::prompt::Message> {
        let thread = &self.threads[self.current_thread_idx];
        thread
            .message_indices
            .last()
            .map(|&idx| &self.messages[idx])
    }

    pub fn add_user_message(
        &mut self,
        now: DateTime<Utc>,
        source: UserMessageSource,
        text: String,
    ) -> Result<Idempotent<llm::Prompt>, AgentSessionError> {
        // If `reset_time_delta_seconds` is configured AND the current thread
        // has already received at least one user message AND that message is
        // older than the threshold, retire the thread and start a fresh one.
        if let Some(delta) = self.reset_time_delta_seconds {
            if let Some(last_at) = self.last_user_message_at() {
                if delta.should_reset(last_at, now) {
                    let previous_thread = self.threads[self.current_thread_idx].id;
                    self.start_new_thread(ThreadStartReason::TimeDeltaExceeded { previous_thread });
                }
            }
        }

        // Idempotency / consecutive check against current thread's last message
        if let Some(last_msg) = self.current_thread_last_message() {
            match last_msg {
                llm::prompt::Message::User { content }
                    if matches!(
                        content.first(),
                        Some(llm::prompt::UserBlock::Text { text: prev, .. }) if prev == &text
                    ) =>
                {
                    return Ok(Idempotent::AlreadyApplied);
                }
                llm::prompt::Message::User { .. } => {
                    return Err(AgentSessionError::ConsecutiveUserMessages);
                }
                _ => {}
            }
        }

        self.events.push(AgentSessionEvent::UserMessage {
            source,
            text: text.clone(),
        });

        let idx = self.messages.len();
        self.messages.push(llm::prompt::Message::User {
            content: vec![llm::prompt::UserBlock::Text {
                text,
                cache_control: None,
            }],
        });
        self.threads[self.current_thread_idx]
            .message_indices
            .push(idx);

        Ok(Idempotent::Executed(self.build_prompt()))
    }

    pub fn add_prompt_response(
        &mut self,
        response: llm::PromptResponse,
    ) -> Vec<llm::RequestToolUse> {
        self.events.push(AgentSessionEvent::AssistantResponse {
            content: response.content.clone(),
        });

        let idx = self.messages.len();
        self.messages.push(llm::prompt::Message::Assistant {
            content: response.content.clone(),
        });
        self.threads[self.current_thread_idx]
            .message_indices
            .push(idx);

        response
            .content
            .into_iter()
            .filter_map(|block| match block {
                AssistantBlock::ToolUse {
                    id, name, input, ..
                } => Some(llm::RequestToolUse { id, name, input }),
                _ => None,
            })
            .collect()
    }

    pub fn add_tool_results(&mut self, results: Vec<llm::ToolUseResult>) -> llm::Prompt {
        let blocks: Vec<llm::prompt::UserBlock> = results
            .iter()
            .map(|r| llm::prompt::UserBlock::ToolResult {
                tool_use_id: r.tool_use_id.clone(),
                content: vec![llm::prompt::ToolResultBlock::Text {
                    text: r.content.clone(),
                }],
                is_error: r.is_error,
                cache_control: None,
            })
            .collect();

        self.events.push(AgentSessionEvent::ToolResults { results });

        let idx = self.messages.len();
        self.messages
            .push(llm::prompt::Message::User { content: blocks });
        self.threads[self.current_thread_idx]
            .message_indices
            .push(idx);

        self.build_prompt()
    }
}

impl TryFromEvents<AgentSessionEvent> for AgentSession {
    fn try_from_events(
        events: EntityEvents<AgentSessionEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = AgentSessionBuilder::default();
        let mut messages: Vec<llm::prompt::Message> = Vec::new();
        let mut threads: Vec<ThreadView> = Vec::new();

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
                AgentSessionEvent::ThreadStarted {
                    thread_id,
                    start_reason,
                } => {
                    threads.push(ThreadView {
                        id: *thread_id,
                        start_reason: *start_reason,
                        message_indices: Vec::new(),
                    });
                }
                AgentSessionEvent::UserMessage { text, .. } => {
                    let idx = messages.len();
                    messages.push(llm::prompt::Message::User {
                        content: vec![llm::prompt::UserBlock::Text {
                            text: text.clone(),
                            cache_control: None,
                        }],
                    });
                    if let Some(thread) = threads.last_mut() {
                        thread.message_indices.push(idx);
                    }
                }
                AgentSessionEvent::AssistantResponse { content } => {
                    let idx = messages.len();
                    messages.push(llm::prompt::Message::Assistant {
                        content: content.clone(),
                    });
                    if let Some(thread) = threads.last_mut() {
                        thread.message_indices.push(idx);
                    }
                }
                AgentSessionEvent::ToolResults { results } => {
                    let blocks: Vec<llm::prompt::UserBlock> = results
                        .iter()
                        .map(|r| llm::prompt::UserBlock::ToolResult {
                            tool_use_id: r.tool_use_id.clone(),
                            content: vec![llm::prompt::ToolResultBlock::Text {
                                text: r.content.clone(),
                            }],
                            is_error: r.is_error,
                            cache_control: None,
                        })
                        .collect();
                    let idx = messages.len();
                    messages.push(llm::prompt::Message::User { content: blocks });
                    if let Some(thread) = threads.last_mut() {
                        thread.message_indices.push(idx);
                    }
                }
            }
        }

        let current_thread_idx = if threads.is_empty() {
            0
        } else {
            threads.len() - 1
        };

        builder
            .messages(messages)
            .threads(threads)
            .current_thread_idx(current_thread_idx)
            .events(events)
            .build()
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

#[cfg(test)]
mod tests {
    use crate::primitives::UserId;
    use es_entity::{IntoEvents as _, TryFromEvents as _};
    use llm::prompt::{Message, UserBlock};

    use super::*;

    fn new_session() -> AgentSession {
        let new = NewAgentSession::builder()
            .agent_id(AgentId::new())
            .model("test-model")
            .system(Vec::new())
            .tools(Vec::new())
            .max_tokens(1024u32)
            .build()
            .expect("NewAgentSession build");
        let mut session =
            AgentSession::try_from_events(new.into_events()).expect("hydrate session");
        let _ = session.init_initial_thread();
        session
    }

    fn user_source() -> UserMessageSource {
        UserMessageSource::User {
            user_id: UserId::new(),
        }
    }

    #[test]
    fn add_user_message_returns_prompt_with_single_user_text_block() {
        let mut session = new_session();

        let prompt = session
            .add_user_message(Utc::now(), user_source(), "Hello".to_string())
            .expect("add_user_message")
            .unwrap();

        assert_eq!(prompt.messages.len(), 1);
        match &prompt.messages[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    UserBlock::Text { text, .. } => assert_eq!(text, "Hello"),
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn two_user_messages_in_a_row_is_rejected() {
        let mut session = new_session();

        let _ = session
            .add_user_message(Utc::now(), user_source(), "first".to_string())
            .expect("first message accepted");

        match session.add_user_message(Utc::now(), user_source(), "second".to_string()) {
            Err(AgentSessionError::ConsecutiveUserMessages) => {}
            Err(e) => panic!("expected ConsecutiveUserMessages, got error: {e}"),
            Ok(_) => panic!("expected ConsecutiveUserMessages error, got Ok"),
        }
    }

    #[test]
    fn repeating_the_same_user_message_is_idempotent() {
        let mut session = new_session();
        let source = user_source();

        let _ = session
            .add_user_message(Utc::now(), source, "Hello".to_string())
            .expect("first call");

        let result = session
            .add_user_message(Utc::now(), source, "Hello".to_string())
            .expect("second call");

        assert!(matches!(result, Idempotent::AlreadyApplied));
    }
}
