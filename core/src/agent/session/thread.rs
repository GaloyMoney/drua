use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;
use llm::prompt::{AssistantBlock, SystemBlock, Tool};
use crate::primitives::UserMessageSource;

use super::{error::AgentSessionError, AgentSessionId};

es_entity::entity_id! { SessionThreadId }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStartReason {
    InitialThread,
    TimeDeltaExceeded { previous_thread: SessionThreadId },
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SessionThreadId")]
pub enum SessionThreadEvent {
    Initialized {
        id: SessionThreadId,
        session_id: AgentSessionId,
        start_reason: ThreadStartReason,
        model: String,
        system: Vec<SystemBlock>,
        tools: Vec<Tool>,
        max_tokens: u32,
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
pub struct SessionThread {
    pub id: SessionThreadId,
    pub session_id: AgentSessionId,
    pub start_reason: ThreadStartReason,
    #[builder(default)]
    pub prompt_state: llm::Prompt,
    events: EntityEvents<SessionThreadEvent>,
}

impl SessionThread {
    pub fn add_user_message(
        &mut self,
        source: UserMessageSource,
        text: String,
    ) -> Result<Idempotent<llm::Prompt>, AgentSessionError> {
        match self.prompt_state.messages.last() {
            Some(llm::prompt::Message::User { content })
                if matches!(
                    content.first(),
                    Some(llm::prompt::UserBlock::Text { text: prev, .. }) if prev == &text
                ) =>
            {
                return Ok(Idempotent::AlreadyApplied);
            }
            Some(llm::prompt::Message::User { .. }) => {
                return Err(AgentSessionError::ConsecutiveUserMessages);
            }
            _ => {}
        }

        self.events.push(SessionThreadEvent::UserMessage {
            source,
            text: text.clone(),
        });
        self.prompt_state.messages.push(llm::prompt::Message::User {
            content: vec![llm::prompt::UserBlock::Text {
                text,
                cache_control: None,
            }],
        });

        Ok(Idempotent::Executed(self.prompt_state.clone()))
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

        self.events
            .push(SessionThreadEvent::ToolResults { results });
        self.prompt_state
            .messages
            .push(llm::prompt::Message::User { content: blocks });
        self.prompt_state.clone()
    }

    /// Timestamp of the most recent persisted `UserMessage` event, if any.
    /// Returns `None` while the thread has only its `Initialized` event.
    pub fn last_user_message_at(&self) -> Option<DateTime<Utc>> {
        self.events
            .iter_persisted()
            .rev()
            .find(|e| matches!(e.event, SessionThreadEvent::UserMessage { .. }))
            .map(|e| e.recorded_at)
    }

    pub fn add_prompt_response(
        &mut self,
        response: llm::PromptResponse,
    ) -> Vec<llm::RequestToolUse> {
        self.events.push(SessionThreadEvent::AssistantResponse {
            content: response.content.clone(),
        });
        self.prompt_state
            .messages
            .push(llm::prompt::Message::Assistant {
                content: response.content.clone(),
            });

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
}

impl TryFromEvents<SessionThreadEvent> for SessionThread {
    fn try_from_events(
        events: EntityEvents<SessionThreadEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = SessionThreadBuilder::default();
        let mut prompt_state = llm::Prompt::default();

        for event in events.iter_all() {
            match event {
                SessionThreadEvent::Initialized {
                    id,
                    session_id,
                    start_reason,
                    model,
                    system,
                    tools,
                    max_tokens,
                } => {
                    builder = builder
                        .id(*id)
                        .session_id(*session_id)
                        .start_reason(*start_reason);
                    prompt_state.model = model.clone();
                    prompt_state.system = system.clone();
                    prompt_state.tools = tools.clone();
                    prompt_state.max_tokens = Some(*max_tokens);
                }
                SessionThreadEvent::UserMessage { text, .. } => {
                    prompt_state.messages.push(llm::prompt::Message::User {
                        content: vec![llm::prompt::UserBlock::Text {
                            text: text.clone(),
                            cache_control: None,
                        }],
                    });
                }
                SessionThreadEvent::AssistantResponse { content, .. } => {
                    prompt_state.messages.push(llm::prompt::Message::Assistant {
                        content: content.clone(),
                    });
                }
                SessionThreadEvent::ToolResults { results } => {
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
                    prompt_state
                        .messages
                        .push(llm::prompt::Message::User { content: blocks });
                }
            }
        }

        builder.prompt_state(prompt_state).events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewSessionThread {
    #[builder(setter(into))]
    pub(super) id: SessionThreadId,
    pub(super) session_id: AgentSessionId,
    pub(super) start_reason: ThreadStartReason,
    #[builder(setter(into))]
    pub(super) model: String,
    pub(super) system: Vec<SystemBlock>,
    pub(super) tools: Vec<Tool>,
    pub(super) max_tokens: u32,
}

impl NewSessionThread {
    pub fn builder() -> NewSessionThreadBuilder {
        let mut builder = NewSessionThreadBuilder::default();
        builder.id(SessionThreadId::new());
        builder
    }
}

impl IntoEvents<SessionThreadEvent> for NewSessionThread {
    fn into_events(self) -> EntityEvents<SessionThreadEvent> {
        EntityEvents::init(
            self.id,
            [SessionThreadEvent::Initialized {
                id: self.id,
                session_id: self.session_id,
                start_reason: self.start_reason,
                model: self.model,
                system: self.system,
                tools: self.tools,
                max_tokens: self.max_tokens,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};
    use llm::prompt::{Message, UserBlock};
    use crate::primitives::UserId;

    use super::*;

    fn new_thread() -> SessionThread {
        let new = NewSessionThread::builder()
            .session_id(AgentSessionId::new())
            .start_reason(ThreadStartReason::InitialThread)
            .model("test-model")
            .system(Vec::new())
            .tools(Vec::new())
            .max_tokens(1024u32)
            .build()
            .expect("NewSessionThread build");
        SessionThread::try_from_events(new.into_events()).expect("hydrate")
    }

    fn user_source() -> UserMessageSource {
        UserMessageSource::User {
            user_id: UserId::new(),
        }
    }

    #[test]
    fn add_user_message_returns_prompt_with_single_user_text_block() {
        let mut thread = new_thread();

        let prompt = thread
            .add_user_message(user_source(), "Hello".to_string())
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
        let mut thread = new_thread();

        let _ = thread
            .add_user_message(user_source(), "first".to_string())
            .expect("first message accepted");

        match thread.add_user_message(user_source(), "second".to_string()) {
            Err(AgentSessionError::ConsecutiveUserMessages) => {}
            Err(e) => panic!("expected ConsecutiveUserMessages, got error: {e}"),
            Ok(_) => panic!("expected ConsecutiveUserMessages error, got Ok"),
        }
    }

    #[test]
    fn repeating_the_same_user_message_is_idempotent() {
        let mut thread = new_thread();
        let source = user_source();

        let _ = thread
            .add_user_message(source, "Hello".to_string())
            .expect("first call");

        let result = thread
            .add_user_message(source, "Hello".to_string())
            .expect("second call");

        assert!(matches!(result, Idempotent::AlreadyApplied));
    }
}
