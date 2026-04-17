use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;

use super::{
    error::AgentSessionError, message::*, metadata::*, new_thread::*, settings::*, view::*,
    AgentSessionId,
};

// ============================================================================
// Events
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStartReason {
    InitialThread,
    ToolDefsUpdated,
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
    ToolDefsUpdated {
        tool_defs: Vec<ToolDefinition>,
    },
    SystemBlocksUpdated {
        system_blocks: Vec<SystemBlock>,
    },
    UserInputAdded {
        target: TargetThread,
        source: UserMessageSource,
        text: String,
    },
    ThreadStarted {
        thread_id: SessionThreadId,
        start_reason: ThreadStartReason,
    },
    PromptSent {
        thread_id: SessionThreadId,
        prompt_definition: PromptDefinition,
        user_messages_view: UserMessagesView,
    },
    AssistantResponseReceived {
        thread_id: SessionThreadId,
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

    #[builder(default)]
    current_main_thread: Option<SessionThreadId>,

    events: EntityEvents<AgentSessionEvent>,

    #[es_entity(nested)]
    #[builder(default)]
    pub(super) threads: Nested<SessionThread>,
}

#[derive(Debug, Clone)]
pub struct ToolUseRequest {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug)]
pub enum AgentSessionResponse {
    PromptPending { target: TargetThread },
    AwaitingAssistantResponse,
    Done,
}

impl AgentSession {
    pub fn add_user_input(
        &mut self,
        target: TargetThread,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        self.events.push(AgentSessionEvent::UserInputAdded {
            target,
            source,
            text: prompt,
        });
        let thread_id = match target {
            TargetThread::Main => match self.current_main_thread {
                Some(id) => id,
                None => return Ok(AgentSessionResponse::PromptPending { target }),
            },
            TargetThread::Id(id) => id,
        };
        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        if thread.is_user_turn() {
            Ok(AgentSessionResponse::PromptPending { target })
        } else {
            Ok(AgentSessionResponse::AwaitingAssistantResponse)
        }
    }

    pub fn next_prompt(&mut self, target: TargetThread) -> Result<Prompt, AgentSessionError> {
        let thread_id = match target {
            TargetThread::Main => self.current_main_thread,
            TargetThread::Id(id) => Some(id),
        };
        if thread_id.is_none() {
            let prompt_definition = self.create_initial_thread();
            let thread_id = self.current_main_thread.expect("just created");
            let user_messages_view = prompt_definition.user_messages_view();
            self.events.push(AgentSessionEvent::PromptSent {
                thread_id,
                prompt_definition: prompt_definition.clone(),
                user_messages_view,
            });
            return prompt_definition.into_prompt(target, &self.events);
        }
        let thread_id = thread_id.unwrap();

        // Collect UserMessageIndexes since last PromptSent for this thread (scan backwards)
        let total_user_msgs = self
            .events
            .iter_all()
            .filter(|e| matches!(e, AgentSessionEvent::UserInputAdded { .. }))
            .count();
        let mut user_msg_counter = total_user_msgs;
        let mut pending_indexes = Vec::new();
        for event in self.events.iter_all().rev() {
            match event {
                AgentSessionEvent::PromptSent { thread_id: tid, .. } if *tid == thread_id => {
                    break;
                }
                AgentSessionEvent::UserInputAdded {
                    target: msg_target, ..
                } => {
                    user_msg_counter -= 1;
                    let targets_this = match msg_target {
                        TargetThread::Main => self.current_main_thread == Some(thread_id),
                        TargetThread::Id(id) => *id == thread_id,
                    };
                    if targets_this {
                        pending_indexes.push(UserMessageIndex::new(user_msg_counter));
                    }
                }
                _ => {}
            }
        }
        pending_indexes.reverse();

        let user_messages_view = UserMessagesView {
            indexes: pending_indexes,
        };

        let thread = self
            .threads
            .get_persisted_mut(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        thread.add_user_message(user_messages_view);
        let prompt_definition = thread.prompt_definition();

        let user_messages_view = prompt_definition.user_messages_view();
        self.events.push(AgentSessionEvent::PromptSent {
            thread_id,
            prompt_definition: prompt_definition.clone(),
            user_messages_view,
        });

        prompt_definition.into_prompt(target, &self.events)
    }

    pub fn update_tool_definitions(&mut self, tool_defs: Vec<ToolDefinition>) {
        self.events
            .push(AgentSessionEvent::ToolDefsUpdated { tool_defs });
    }

    pub fn update_system_blocks(&mut self, system_blocks: Vec<SystemBlock>) {
        self.events
            .push(AgentSessionEvent::SystemBlocksUpdated { system_blocks });
    }

    pub fn assistant_response_received(
        &mut self,
        thread_id: SessionThreadId,
        content: Vec<AssistantBlock>,
        stop_reason: StopReason,
        error_message: Option<String>,
        metadata: AssistantResponseMetadata,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        if thread.is_user_turn() {
            return Err(AgentSessionError::NotAssistantTurn);
        }

        self.events
            .push(AgentSessionEvent::AssistantResponseReceived {
                thread_id,
                content,
                stop_reason,
                error_message,
                metadata,
            });

        let view = self.materialize().assistant_blocks_since_last_breakpoint();

        let thread = self
            .threads
            .get_persisted_mut(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        thread.add_assistant_message(view);

        // Check if user input arrived after the last prompt was sent for this thread
        let has_pending_input = self
            .events
            .iter_all()
            .rev()
            .take_while(|e| {
                !matches!(e, AgentSessionEvent::PromptSent { thread_id: tid, .. } if *tid == thread_id)
            })
            .any(|e| match e {
                AgentSessionEvent::UserInputAdded { target, .. } => match target {
                    TargetThread::Main => self.current_main_thread == Some(thread_id),
                    TargetThread::Id(id) => *id == thread_id,
                },
                _ => false,
            });

        if has_pending_input {
            let target = if self.current_main_thread == Some(thread_id) {
                TargetThread::Main
            } else {
                TargetThread::Id(thread_id)
            };
            Ok(AgentSessionResponse::PromptPending { target })
        } else {
            Ok(AgentSessionResponse::Done)
        }
    }

    fn create_initial_thread(&mut self) -> PromptDefinition {
        let prompt_definition = self.materialize().initial_prompt_definition();
        let thread_id = SessionThreadId::new();
        let new_thread = NewSessionThread::builder()
            .id(thread_id)
            .session_id(self.id)
            .model(prompt_definition.model.clone())
            .system_view(prompt_definition.system_view().clone())
            .tool_definitions_view(prompt_definition.tool_definitions_view().clone())
            .initial_user_messages(prompt_definition.user_messages_view())
            .build()
            .expect("NewSessionThread build");
        self.threads.add_new(new_thread);
        self.events.push(AgentSessionEvent::ThreadStarted {
            thread_id,
            start_reason: ThreadStartReason::InitialThread,
        });
        self.current_main_thread = Some(thread_id);
        prompt_definition
    }

    fn materialize(&self) -> MaterializedSession<'_> {
        let mut materialized = MaterializedSession::init("");
        for event in self.events.iter_all() {
            match event {
                AgentSessionEvent::Initialized {
                    model_settings,
                    system_blocks,
                    tool_defs,
                    ..
                } => {
                    materialized = MaterializedSession::init(&model_settings.model);
                    materialized.push_system_blocks(system_blocks.iter());
                    materialized.push_tool_defs(tool_defs.iter());
                }
                AgentSessionEvent::ToolDefsUpdated { tool_defs } => {
                    materialized.push_tool_defs(tool_defs.iter());
                }
                AgentSessionEvent::SystemBlocksUpdated { system_blocks } => {
                    materialized.push_system_blocks(system_blocks.iter());
                }
                AgentSessionEvent::UserInputAdded { .. } => {
                    materialized.push_user_message();
                }
                AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                    materialized.push_assistant_blocks(content.len());
                }
                _ => {}
            }
        }
        materialized
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
                    builder = builder.current_main_thread(Some(*thread_id));
                }
                AgentSessionEvent::UserInputAdded { .. } => {}
                AgentSessionEvent::AssistantResponseReceived { .. } => {}
                AgentSessionEvent::ToolDefsUpdated { .. } => {}
                AgentSessionEvent::SystemBlocksUpdated { .. } => {}
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
    use es_entity::{IntoEvents as _, TryFromEvents as _};

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

    fn hydrate_threads(session: &mut AgentSession) {
        let new_threads = session
            .threads
            .new_entities_mut()
            .drain(..)
            .map(|new| SessionThread::try_from_events(new.into_events()).expect("hydrate thread"))
            .collect::<Vec<_>>();
        session.threads.load(new_threads);
    }

    fn dummy_metadata() -> AssistantResponseMetadata {
        AssistantResponseMetadata {
            api: "test".into(),
            model: "test-model".into(),
            usage: Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 0,
            },
            cost: Cost::default(),
        }
    }

    fn user_source() -> UserMessageSource {
        UserMessageSource::User {
            user_id: UserId::new(),
        }
    }

    #[test]
    fn add_user_message_returns_prompt_pending_when_no_thread() {
        let mut session = new_session();
        assert!(session.current_main_thread.is_none());

        let result = session.add_user_input(TargetThread::Main, user_source(), "Hello".into());
        assert!(matches!(
            result,
            Ok(AgentSessionResponse::PromptPending {
                target: TargetThread::Main
            })
        ));
    }

    #[test]
    fn add_user_messages_return_prompt_pending() {
        let mut session = new_session();

        let messages = ["Hello", "How are you?", "Tell me about Rust"];
        for msg in messages {
            let result = session.add_user_input(TargetThread::Main, user_source(), msg.into());
            assert!(
                matches!(result, Ok(AgentSessionResponse::PromptPending { .. })),
                "expected PromptPending for '{msg}'; got {result:?}"
            );
        }
    }

    #[test]
    fn assistant_text_response_flips_thread_to_user_turn() {
        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session
            .next_prompt(TargetThread::Main)
            .expect("next_prompt should succeed");

        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        let result = session
            .assistant_response_received(
                thread_id,
                vec![AssistantBlock::Text {
                    text: "Hi there!".into(),
                }],
                StopReason::Stop,
                None,
                dummy_metadata(),
            )
            .unwrap();

        assert!(matches!(result, AgentSessionResponse::Done));

        // Second response on same thread should fail — it's now user's turn
        let result = session.assistant_response_received(
            thread_id,
            vec![AssistantBlock::Text {
                text: "Unexpected".into(),
            }],
            StopReason::Stop,
            None,
            dummy_metadata(),
        );
        assert!(matches!(result, Err(AgentSessionError::NotAssistantTurn)));
    }

    #[test]
    fn assistant_response_returns_prompt_pending_when_user_input_queued() {
        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();

        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        // User sends follow-up while assistant is working
        let result = session
            .add_user_input(TargetThread::Main, user_source(), "Also check X".into())
            .unwrap();
        assert!(matches!(
            result,
            AgentSessionResponse::AwaitingAssistantResponse
        ));

        // Assistant responds — should signal PromptPending because of queued input
        let result = session
            .assistant_response_received(
                thread_id,
                vec![AssistantBlock::Text {
                    text: "Hi there!".into(),
                }],
                StopReason::Stop,
                None,
                dummy_metadata(),
            )
            .unwrap();

        assert!(matches!(
            result,
            AgentSessionResponse::PromptPending {
                target: TargetThread::Main
            }
        ));

        // Now call next_prompt — should build prompt with the queued message
        let prompt = session
            .next_prompt(TargetThread::Main)
            .expect("next_prompt for existing thread");

        // Prompt should contain the full conversation: initial user, assistant, follow-up
        assert_eq!(prompt.messages.len(), 3);
        match &prompt.messages[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(&content[0], UserBlock::Text { text } if text == "Hello"));
            }
            _ => panic!("expected User message"),
        }
        match &prompt.messages[1] {
            Message::Assistant { content } => {
                assert_eq!(content.len(), 1);
                assert!(
                    matches!(&content[0], AssistantBlock::Text { text } if text == "Hi there!")
                );
            }
            _ => panic!("expected Assistant message"),
        }
        match &prompt.messages[2] {
            Message::User { content } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(&content[0], UserBlock::Text { text } if text == "Also check X"));
            }
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn next_prompt_creates_thread_and_returns_prompt() {
        let mut session = new_session();

        session.update_system_blocks(vec![SystemBlock::Text {
            text: "You are helpful.".into(),
        }]);
        session.update_tool_definitions(vec![ToolDefinition {
            name: "get_weather".into(),
            description: Some("Get weather".into()),
            input_schema: serde_json::json!({"type": "object"}),
        }]);

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        session
            .add_user_input(
                TargetThread::Main,
                user_source(),
                "What's the weather?".into(),
            )
            .unwrap();

        let prompt = session
            .next_prompt(TargetThread::Main)
            .expect("next_prompt should succeed");

        assert_eq!(prompt.model, "test-model");

        assert_eq!(prompt.system.len(), 1);
        assert!(
            matches!(&prompt.system[0], SystemBlock::Text { text } if text == "You are helpful.")
        );

        assert_eq!(prompt.tools.len(), 1);
        assert_eq!(prompt.tools[0].name, "get_weather");

        assert_eq!(prompt.messages.len(), 1);
        match &prompt.messages[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 2);
                assert!(matches!(&content[0], UserBlock::Text { text } if text == "Hello"));
                assert!(
                    matches!(&content[1], UserBlock::Text { text } if text == "What's the weather?")
                );
            }
            _ => panic!("expected User message"),
        }

        assert!(session.current_main_thread.is_some());

        hydrate_threads(&mut session);

        let result =
            session.add_user_input(TargetThread::Main, user_source(), "Another message".into());
        assert!(matches!(
            result,
            Ok(AgentSessionResponse::AwaitingAssistantResponse)
        ));
    }
}
