use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;

use super::{
    error::AgentSessionError, message::*, metadata::*, settings::*, thread::*, view::*,
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
    SandboxNotificationAdded {
        target: TargetThread,
        sandbox_name: String,
        operation: SandboxOperation,
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
    ToolResultsAdded {
        thread_id: SessionThreadId,
        results: Vec<ToolResultInput>,
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
    AwaitingToolUsageComplete,
    ToolUseRequest(Vec<ToolUseRequest>),
    Done,
}

impl AgentSession {
    pub fn current_main_thread_id(&self) -> Option<SessionThreadId> {
        self.current_main_thread
    }

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
        self.user_message_response(target)
    }

    pub fn add_sandbox_notification(
        &mut self,
        sandbox_name: String,
        operation: SandboxOperation,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let target = TargetThread::Main;
        self.events
            .push(AgentSessionEvent::SandboxNotificationAdded {
                target,
                sandbox_name,
                operation,
            });
        self.user_message_response(target)
    }

    /// Common post-push response logic shared by [`Self::add_user_input`]
    /// and [`Self::add_sandbox_notification`].
    fn user_message_response(
        &self,
        target: TargetThread,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
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
        } else if thread.is_tool_use_turn() {
            Ok(AgentSessionResponse::AwaitingToolUsageComplete)
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

        // Collect pending BlockIndexes since last PromptSent for this thread (scan backwards)
        let total_blocks = self.events.iter_all().fold(0usize, |acc, e| match e {
            AgentSessionEvent::UserInputAdded { .. }
            | AgentSessionEvent::SandboxNotificationAdded { .. } => acc + 1,
            AgentSessionEvent::AssistantResponseReceived { content, .. } => acc + content.len(),
            AgentSessionEvent::ToolResultsAdded { results, .. } => acc + results.len(),
            _ => acc,
        });
        let mut block_counter = total_blocks;
        let mut pending_indexes = Vec::new();
        for event in self.events.iter_all().rev() {
            match event {
                AgentSessionEvent::PromptSent { thread_id: tid, .. } if *tid == thread_id => {
                    break;
                }
                AgentSessionEvent::UserInputAdded {
                    target: msg_target, ..
                }
                | AgentSessionEvent::SandboxNotificationAdded {
                    target: msg_target, ..
                } => {
                    block_counter -= 1;
                    let targets_this = match msg_target {
                        TargetThread::Main => self.current_main_thread == Some(thread_id),
                        TargetThread::Id(id) => *id == thread_id,
                    };
                    if targets_this {
                        pending_indexes.push(MessageBlockIndex::new(block_counter));
                    }
                }
                AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                    block_counter -= content.len();
                }
                AgentSessionEvent::ToolResultsAdded { results, .. } => {
                    block_counter -= results.len();
                }
                _ => {}
            }
        }
        pending_indexes.reverse();

        if !pending_indexes.is_empty() {
            let user_messages_view = UserMessagesView {
                indexes: pending_indexes,
            };
            let thread = self
                .threads
                .get_persisted_mut(&thread_id)
                .ok_or(AgentSessionError::ThreadNotFound)?;
            thread.add_user_message(user_messages_view);
        }

        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
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

    pub fn add_tool_results(
        &mut self,
        thread_id: SessionThreadId,
        results: Vec<ToolResultInput>,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        if !thread.is_tool_use_turn() {
            return Err(AgentSessionError::NotToolUseTurn);
        }

        self.events
            .push(AgentSessionEvent::ToolResultsAdded { thread_id, results });

        let view = self.materialize().tool_results_since_last_breakpoint();
        let thread = self
            .threads
            .get_persisted_mut(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        thread.add_tool_results(view);

        let target = if self.current_main_thread == Some(thread_id) {
            TargetThread::Main
        } else {
            TargetThread::Id(thread_id)
        };
        Ok(AgentSessionResponse::PromptPending { target })
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
        if !thread.is_assistant_turn() {
            return Err(AgentSessionError::NotAssistantTurn);
        }

        // Extract tool use requests before content is moved into the event
        let tool_uses: Vec<ToolUseRequest> = content
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::ToolUse { id, name, input } => Some(ToolUseRequest {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect();
        let is_tool_use = matches!(stop_reason, StopReason::ToolUse) && !tool_uses.is_empty();

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

        if is_tool_use {
            thread.add_assistant_tool_use(view);
            return Ok(AgentSessionResponse::ToolUseRequest(tool_uses));
        }

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
                AgentSessionEvent::UserInputAdded { target, .. }
                | AgentSessionEvent::SandboxNotificationAdded { target, .. } => match target {
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
            .max_tokens(prompt_definition.max_tokens)
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
        let mut materialized = MaterializedSession::init("", 0);
        for event in self.events.iter_all() {
            match event {
                AgentSessionEvent::Initialized {
                    model_settings,
                    system_blocks,
                    tool_defs,
                    ..
                } => {
                    materialized =
                        MaterializedSession::init(&model_settings.model, model_settings.max_tokens);
                    materialized.push_system_blocks(system_blocks.iter());
                    materialized.push_tool_defs(tool_defs.iter());
                }
                AgentSessionEvent::ToolDefsUpdated { tool_defs } => {
                    materialized.push_tool_defs(tool_defs.iter());
                }
                AgentSessionEvent::SystemBlocksUpdated { system_blocks } => {
                    materialized.push_system_blocks(system_blocks.iter());
                }
                AgentSessionEvent::UserInputAdded { .. }
                | AgentSessionEvent::SandboxNotificationAdded { .. } => {
                    materialized.push_user_message();
                }
                AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                    materialized.push_assistant_blocks(content.len());
                }
                AgentSessionEvent::ToolResultsAdded { results, .. } => {
                    materialized.push_tool_results(results.len());
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
                AgentSessionEvent::SandboxNotificationAdded { .. } => {}
                AgentSessionEvent::AssistantResponseReceived { .. } => {}
                AgentSessionEvent::ToolDefsUpdated { .. } => {}
                AgentSessionEvent::SystemBlocksUpdated { .. } => {}
                AgentSessionEvent::PromptSent { .. } => {}
                AgentSessionEvent::ToolResultsAdded { .. } => {}
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
    fn tool_use_flow_returns_tool_requests_then_prompt_with_results() {
        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "Use the tool".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();

        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        // Assistant responds with tool use
        let result = session
            .assistant_response_received(
                thread_id,
                vec![
                    AssistantBlock::Text {
                        text: "Let me check.".into(),
                    },
                    AssistantBlock::ToolUse {
                        id: "tool_1".into(),
                        name: "get_weather".into(),
                        input: serde_json::json!({"city": "NYC"}),
                    },
                ],
                StopReason::ToolUse,
                None,
                dummy_metadata(),
            )
            .unwrap();

        // Should return ToolUseRequest
        match &result {
            AgentSessionResponse::ToolUseRequest(requests) => {
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].id, "tool_1");
                assert_eq!(requests[0].name, "get_weather");
            }
            other => panic!("expected ToolUseRequest, got {other:?}"),
        }

        // Thread should be in tool use state — assistant response should fail
        let err = session.assistant_response_received(
            thread_id,
            vec![AssistantBlock::Text {
                text: "Oops".into(),
            }],
            StopReason::Stop,
            None,
            dummy_metadata(),
        );
        assert!(matches!(err, Err(AgentSessionError::NotAssistantTurn)));

        // User sends a message while waiting for tool results
        let result = session
            .add_user_input(TargetThread::Main, user_source(), "Also check LA".into())
            .unwrap();
        assert!(matches!(
            result,
            AgentSessionResponse::AwaitingToolUsageComplete
        ));

        // Add tool results
        let result = session
            .add_tool_results(
                thread_id,
                vec![ToolResultInput {
                    tool_use_id: "tool_1".into(),
                    content: "Sunny, 72°F".into(),
                    is_error: false,
                }],
            )
            .unwrap();
        assert!(matches!(
            result,
            AgentSessionResponse::PromptPending {
                target: TargetThread::Main
            }
        ));

        // Now get the prompt — should include user, assistant (with tool use), and tool results
        let prompt = session
            .next_prompt(TargetThread::Main)
            .expect("next_prompt after tool results");

        assert_eq!(prompt.messages.len(), 3);

        // First message: user
        match &prompt.messages[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(&content[0], UserBlock::Text { text } if text == "Use the tool"));
            }
            _ => panic!("expected User message"),
        }

        // Second message: assistant with text + tool use
        match &prompt.messages[1] {
            Message::Assistant { content } => {
                assert_eq!(content.len(), 2);
                assert!(
                    matches!(&content[0], AssistantBlock::Text { text } if text == "Let me check.")
                );
                assert!(matches!(
                    &content[1],
                    AssistantBlock::ToolUse {
                        id,
                        name,
                        ..
                    } if id == "tool_1" && name == "get_weather"
                ));
            }
            _ => panic!("expected Assistant message"),
        }

        // Third message: tool results + queued user text merged into one User message
        match &prompt.messages[2] {
            Message::User { content } => {
                assert_eq!(content.len(), 2);
                match &content[0] {
                    UserBlock::ToolResult {
                        tool_use_id,
                        content: blocks,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "tool_1");
                        assert!(!is_error);
                        assert_eq!(blocks.len(), 1);
                        assert!(
                            matches!(&blocks[0], ToolResultBlock::Text { text } if text == "Sunny, 72°F")
                        );
                    }
                    _ => panic!("expected ToolResult block"),
                }
                assert!(matches!(&content[1], UserBlock::Text { text } if text == "Also check LA"));
            }
            _ => panic!("expected User message with tool results and queued text"),
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
