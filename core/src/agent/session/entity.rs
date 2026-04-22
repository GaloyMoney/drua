use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::{AgentId, UserMessageSource};
use es_entity::*;

use crate::agent::config::ModelDefaults;

use super::{
    compaction, error::AgentSessionError, export, history, message::*, metadata::*, settings::*,
    thread::*, view::*, AgentSessionId,
};

// ============================================================================
// Events
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStartReason {
    InitialThread,
    ToolDefsUpdated,
    Compaction { from_thread: SessionThreadId },
    Orphan { from_thread: SessionThreadId },
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentSessionId")]
pub enum AgentSessionEvent {
    Initialized {
        id: AgentSessionId,
        agent_id: AgentId,
        model_defaults: ModelDefaults,
        compaction_config: CompactionConfig,
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
        /// Pre-computed notification text. Persisted so the chat history
        /// remains accurate even if the template changes in a future version.
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
    ToolResultsAdded {
        thread_id: SessionThreadId,
        results: Vec<ToolResultInput>,
    },
    /// Masked tool results pushed into the main event stream so they
    /// participate in block indexing and can be shared across threads.
    ToolResultsMasked {
        results: Vec<MaskedToolResult>,
    },
    CompactionApplied {
        from_thread_id: SessionThreadId,
        new_thread_id: SessionThreadId,
        masked_tool_results: Vec<MessageBlockIndex>,
        cleared_thinking: Vec<MessageBlockIndex>,
        stripped_user_messages: Vec<MessageBlockIndex>,
        estimated_tokens_saved: u64,
    },
}

/// A tool result that was masked during compaction, carrying both the
/// original view index and the replacement content (placeholder text).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaskedToolResult {
    pub original_index: MessageBlockIndex,
    pub replacement: ToolResultInput,
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub agent_id: AgentId,

    #[builder(default)]
    current_main_thread: Option<SessionThreadId>,

    #[builder(default)]
    model_defaults: ModelDefaults,

    #[builder(default)]
    compaction_config: CompactionConfig,

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

    /// Build a format-agnostic [`ExportableThread`] for the given target.
    ///
    /// Defaults to the current main thread when `target` is `Main`.
    /// Format-specific modules (e.g. `pi_export`) consume the returned
    /// intermediate representation to produce their output.
    pub fn exportable_thread(
        &self,
        target: TargetThread,
    ) -> Result<export::ExportableThread, AgentSessionError> {
        let thread_id = match target {
            TargetThread::Main => self
                .current_main_thread
                .ok_or(AgentSessionError::ThreadNotFound)?,
            TargetThread::Id(id) => id,
        };

        Ok(export::build_exportable_thread(
            self.id,
            &self.model_defaults.model,
            &self.events,
            thread_id,
            self.current_main_thread,
        ))
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
        let text = sandbox_notification_text(&sandbox_name, &operation);
        self.events
            .push(AgentSessionEvent::SandboxNotificationAdded {
                target,
                sandbox_name,
                operation,
                text,
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
            return self.build_prompt(target, prompt_definition);
        }
        let thread_id = thread_id.unwrap();

        // Collect pending BlockIndexes since last PromptSent for this thread (scan backwards)
        let total_blocks = self.events.iter_all().fold(0usize, |acc, e| match e {
            AgentSessionEvent::UserInputAdded { .. }
            | AgentSessionEvent::SandboxNotificationAdded { .. } => acc + 1,
            AgentSessionEvent::AssistantResponseReceived { content, .. } => acc + content.len(),
            AgentSessionEvent::ToolResultsAdded { results, .. } => acc + results.len(),
            AgentSessionEvent::ToolResultsMasked { results, .. } => acc + results.len(),
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
                AgentSessionEvent::ToolResultsMasked { results, .. } => {
                    block_counter -= results.len();
                }
                _ => {}
            }
        }
        pending_indexes.reverse();

        // Build prompt definition from current thread WITHOUT adding
        // the new user messages to the thread yet. We augment the
        // prompt_definition directly so compaction can see the full
        // context, but the old thread won't carry the user message
        // if compaction creates a new thread (avoiding duplication).
        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        let mut prompt_definition = thread.prompt_definition();

        let pending_user_view = if !pending_indexes.is_empty() {
            let umv = UserMessagesView {
                indexes: pending_indexes,
            };
            prompt_definition
                .messages
                .push(MessageView::User(umv.clone()));
            Some(umv)
        } else {
            None
        };

        // --- Compaction check ---
        let (thread_id, prompt_definition) = match self.try_prune(thread_id, &prompt_definition) {
            Some(result) => result, // user messages carried to compacted thread
            None => {
                // No compaction — now add user messages to the old thread
                if let Some(umv) = pending_user_view {
                    let thread = self
                        .threads
                        .get_persisted_mut(&thread_id)
                        .ok_or(AgentSessionError::ThreadNotFound)?;
                    thread.add_user_message(umv);
                }
                (thread_id, prompt_definition)
            }
        };
        let target = if self.current_main_thread == Some(thread_id) {
            TargetThread::Main
        } else {
            TargetThread::Id(thread_id)
        };

        let user_messages_view = prompt_definition.user_messages_view();
        self.events.push(AgentSessionEvent::PromptSent {
            thread_id,
            prompt_definition: prompt_definition.clone(),
            user_messages_view,
        });

        self.build_prompt(target, prompt_definition)
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

        // Strip orphaned tool_use blocks when we won't execute them (e.g.
        // stop_reason is EndTurn/MaxTokens/None). Persisting tool_use blocks
        // without matching tool_result blocks causes the next prompt to be
        // rejected by the API.
        let content = if !is_tool_use && !tool_uses.is_empty() {
            content
                .into_iter()
                .filter(|b| !matches!(b, AssistantBlock::ToolUse { .. }))
                .collect()
        } else {
            content
        };

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

    /// Attempt pruning compaction for the given thread. Returns the new
    /// thread id and prompt definition if compaction was applied, or `None`
    /// if no compaction was needed.
    fn try_prune(
        &mut self,
        current_thread_id: SessionThreadId,
        current_prompt_def: &PromptDefinition,
    ) -> Option<(SessionThreadId, PromptDefinition)> {
        let is_main_thread = self.current_main_thread == Some(current_thread_id);
        let result = compaction::maybe_prune(
            &self.events,
            &self.compaction_config,
            &self.model_defaults,
            self.id,
            current_thread_id,
            is_main_thread,
            current_prompt_def,
        )?;

        // Apply compaction: push events and add new thread
        for event in result.events {
            self.events.push(event);
        }
        self.threads.add_new(result.new_thread);
        self.current_main_thread = Some(result.new_thread_id);

        Some((result.new_thread_id, result.prompt_definition))
    }

    fn create_initial_thread(&mut self) -> PromptDefinition {
        let prompt_definition = self.materialize().initial_prompt_definition();
        let thread_id = SessionThreadId::new();
        let new_thread = NewSessionThread::builder()
            .id(thread_id)
            .session_id(self.id)
            .start_reason(ThreadStartReason::InitialThread)
            .model_defaults(self.model_defaults.clone())
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

    fn build_prompt(
        &self,
        target: TargetThread,
        prompt_definition: PromptDefinition,
    ) -> Result<Prompt, AgentSessionError> {
        let mut prompt = prompt_definition.into_prompt(target, &self.events)?;
        prompt.cache_key = Some(format!("agent-session:{}", self.id));
        Ok(prompt)
    }

    fn materialize(&self) -> MaterializedSession<'_> {
        let mut materialized = MaterializedSession::init(&self.model_defaults);
        for event in self.events.iter_all() {
            match event {
                AgentSessionEvent::Initialized {
                    system_blocks,
                    tool_defs,
                    ..
                } => {
                    materialized = MaterializedSession::init(&self.model_defaults);
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
                AgentSessionEvent::ToolResultsMasked { results, .. } => {
                    materialized.push_tool_results(results.len());
                }
                _ => {}
            }
        }
        materialized
    }

    // ─── History query methods ──────────────────────────────────────────────

    pub fn chat_history(&self, last_n: usize) -> Vec<history::ChatHistoryMessage> {
        history::build_chat_history(self.events.iter_all(), last_n)
    }

    pub fn thread_infos(&self) -> Vec<history::SessionThreadInfo> {
        history::build_thread_infos(
            self.threads.iter_persisted(),
            self.events.iter_all(),
            self.current_main_thread,
        )
    }

    pub fn thread_messages(
        &self,
        thread_id: SessionThreadId,
    ) -> Result<Vec<history::ThreadMessage>, AgentSessionError> {
        let thread = self
            .threads
            .get_persisted(&thread_id)
            .ok_or(AgentSessionError::ThreadNotFound)?;
        Ok(history::build_thread_messages(
            thread.prompt_definition(),
            &self.events,
        ))
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
                    model_defaults,
                    compaction_config,
                    ..
                } => {
                    builder = builder
                        .id(*id)
                        .agent_id(*agent_id)
                        .model_defaults(model_defaults.clone())
                        .compaction_config(compaction_config.clone());
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
                AgentSessionEvent::ToolResultsMasked { .. } => {}
                AgentSessionEvent::CompactionApplied { .. } => {}
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
    pub(super) model_defaults: ModelDefaults,
    #[builder(default)]
    pub(super) compaction_config: CompactionConfig,
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
                model_defaults: self.model_defaults,
                compaction_config: self.compaction_config,
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
            .model_defaults(ModelDefaults {
                model: "test-model".into(),
                max_tokens_per_response: 1024,
                context_window_tokens: 200_000,
                ..Default::default()
            })
            .compaction_config(CompactionConfig {
                enabled: false,
                ..Default::default()
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
        let expected_cache_key = format!("agent-session:{}", session.id);
        assert_eq!(
            prompt.cache_key.as_deref(),
            Some(expected_cache_key.as_str())
        );

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

    fn new_session_with_compaction(keep_recent: usize) -> AgentSession {
        let new = NewAgentSession::builder()
            .agent_id(AgentId::new())
            .model_defaults(ModelDefaults {
                model: "test-model".into(),
                max_tokens_per_response: 1024,
                context_window_tokens: 100,
                cache_ttl_seconds: 0, // cache always cold
            })
            .compaction_config(CompactionConfig {
                enabled: true,
                token_threshold_fraction: 0.0, // always over threshold
                keep_recent_tool_results: keep_recent,
                reset_time_delta_seconds: None,
            })
            .system_blocks(vec![SystemBlock::Text {
                text: "You are helpful.".into(),
            }])
            .tool_defs(vec![ToolDefinition {
                name: "get_weather".into(),
                description: Some("Get weather".into()),
                input_schema: serde_json::json!({"type": "object"}),
            }])
            .build()
            .expect("NewAgentSession build");
        AgentSession::try_from_events(new.into_events()).expect("hydrate")
    }

    /// Helper: drive session through a complete tool-use turn (user → prompt →
    /// assistant tool_use → tool_results → prompt → assistant stop).
    ///
    /// Always uses the current main thread — handles thread switches from compaction.
    fn drive_tool_turn(session: &mut AgentSession, tool_result_content: &str) {
        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(session);

        // Assistant responds with tool use
        let tool_id = format!("tool_{}", uuid::Uuid::new_v4());
        session
            .assistant_response_received(
                thread_id,
                vec![
                    AssistantBlock::Text {
                        text: "Let me check.".into(),
                    },
                    AssistantBlock::ToolUse {
                        id: tool_id.clone(),
                        name: "get_weather".into(),
                        input: serde_json::json!({"city": "NYC"}),
                    },
                ],
                StopReason::ToolUse,
                None,
                dummy_metadata(),
            )
            .unwrap();

        // Add tool results (thread_id stays the same here — no compaction mid-turn)
        session
            .add_tool_results(
                thread_id,
                vec![ToolResultInput {
                    tool_use_id: tool_id,
                    content: tool_result_content.into(),
                    is_error: false,
                }],
            )
            .unwrap();

        // Get prompt for tool results turn — this may trigger compaction and
        // switch to a new thread, so re-read current_main_thread after.
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        hydrate_threads(session);

        let thread_id = session.current_main_thread.unwrap();

        // Assistant provides final response
        session
            .assistant_response_received(
                thread_id,
                vec![AssistantBlock::Text {
                    text: "Done.".into(),
                }],
                StopReason::Stop,
                None,
                dummy_metadata(),
            )
            .unwrap();
    }

    #[test]
    fn compaction_disabled_does_not_compact() {
        let mut session = new_session(); // compaction disabled by default

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        let original_thread = session.current_main_thread.unwrap();

        hydrate_threads(&mut session);
        session
            .assistant_response_received(
                original_thread,
                vec![AssistantBlock::Text { text: "Hi".into() }],
                StopReason::Stop,
                None,
                dummy_metadata(),
            )
            .unwrap();

        session
            .add_user_input(TargetThread::Main, user_source(), "Next".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();

        // Thread should NOT have changed (no compaction)
        assert_eq!(session.current_main_thread.unwrap(), original_thread);
    }

    #[test]
    fn compaction_spawns_new_thread_and_preserves_conversation() {
        // keep_recent=1: all but the most recent tool result will be masked
        let mut session = new_session_with_compaction(1);

        // Initial user message + first prompt
        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        hydrate_threads(&mut session);
        let original_thread = session.current_main_thread.unwrap();

        // Drive tool-use turns with large results. Compaction triggers once
        // there are ≥2 tool results on the current thread (keep_recent=1).
        // We check EVERY next_prompt call (including inside the tool-use flow).
        let large_content = "x".repeat(500);
        let mut compacted_prompt = None;

        let check_prompt = |p: &Prompt, cp: &mut Option<Prompt>| {
            if p.compaction.is_some() && cp.is_none() {
                *cp = Some(p.clone());
            }
        };

        for i in 0..5 {
            // User input → prompt (may trigger compaction)
            session
                .add_user_input(TargetThread::Main, user_source(), format!("Turn {i}"))
                .unwrap();
            let prompt = session.next_prompt(TargetThread::Main).unwrap();
            hydrate_threads(&mut session);
            check_prompt(&prompt, &mut compacted_prompt);

            // Assistant tool use
            let thread_id = session.current_main_thread.unwrap();
            let tool_id = format!("tool_{i}");
            session
                .assistant_response_received(
                    thread_id,
                    vec![
                        AssistantBlock::Text {
                            text: "Let me check.".into(),
                        },
                        AssistantBlock::ToolUse {
                            id: tool_id.clone(),
                            name: "get_weather".into(),
                            input: serde_json::json!({"city": "NYC"}),
                        },
                    ],
                    StopReason::ToolUse,
                    None,
                    dummy_metadata(),
                )
                .unwrap();

            // Tool results
            session
                .add_tool_results(
                    thread_id,
                    vec![ToolResultInput {
                        tool_use_id: tool_id,
                        content: large_content.clone(),
                        is_error: false,
                    }],
                )
                .unwrap();

            // Prompt for tool results turn (may trigger compaction)
            let prompt = session.next_prompt(TargetThread::Main).unwrap();
            hydrate_threads(&mut session);
            check_prompt(&prompt, &mut compacted_prompt);

            // Final assistant response
            let thread_id = session.current_main_thread.unwrap();
            session
                .assistant_response_received(
                    thread_id,
                    vec![AssistantBlock::Text {
                        text: "Done.".into(),
                    }],
                    StopReason::Stop,
                    None,
                    dummy_metadata(),
                )
                .unwrap();
        }

        // Compaction must have fired at least once
        let prompt = compacted_prompt.expect("compaction should have fired during tool turns");

        // Should have switched to a new thread
        let new_thread = session.current_main_thread.unwrap();
        assert_ne!(new_thread, original_thread);

        // Prompt should have compaction metadata
        let meta = prompt.compaction.unwrap();
        assert!(meta.tool_results_masked > 0);
        // follows_from should not reference the final thread
        assert_ne!(meta.follows_from, new_thread);

        // Prompt should still have the conversation
        assert!(!prompt.messages.is_empty());
    }

    #[test]
    fn compaction_event_is_recorded() {
        let mut session = new_session_with_compaction(1);

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        hydrate_threads(&mut session);

        let large_content = "x".repeat(500);
        for i in 0..3 {
            session
                .add_user_input(TargetThread::Main, user_source(), format!("Turn {i}"))
                .unwrap();
            let _ = session.next_prompt(TargetThread::Main).unwrap();
            hydrate_threads(&mut session);
            drive_tool_turn(&mut session, &large_content);
        }

        session
            .add_user_input(TargetThread::Main, user_source(), "What next?".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();

        // Should have a CompactionApplied event
        let compaction_events: Vec<_> = session
            .events
            .iter_all()
            .filter(|e| matches!(e, AgentSessionEvent::CompactionApplied { .. }))
            .collect();
        assert!(
            !compaction_events.is_empty(),
            "expected CompactionApplied event"
        );
    }

    // ─── Chat history tests ─────────────────────────────────────────────────

    #[test]
    fn chat_history_returns_user_and_assistant_messages() {
        use super::history::{ChatHistoryBlock, ChatHistoryRole};

        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        session
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

        session
            .add_user_input(TargetThread::Main, user_source(), "How are you?".into())
            .unwrap();

        let history = session.chat_history(10);
        assert_eq!(history.len(), 3);
        assert!(matches!(history[0].role, ChatHistoryRole::User));
        assert!(matches!(history[1].role, ChatHistoryRole::Assistant));
        assert!(matches!(history[2].role, ChatHistoryRole::User));

        // Verify content
        assert!(
            matches!(&history[0].blocks[0], ChatHistoryBlock::Text { text } if text == "Hello")
        );
        assert!(
            matches!(&history[1].blocks[0], ChatHistoryBlock::Text { text } if text == "Hi there!")
        );
        assert!(
            matches!(&history[2].blocks[0], ChatHistoryBlock::Text { text } if text == "How are you?")
        );
    }

    #[test]
    fn chat_history_last_n_limits_results() {
        use super::history::{ChatHistoryBlock, ChatHistoryRole};

        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "First".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        session
            .assistant_response_received(
                thread_id,
                vec![AssistantBlock::Text {
                    text: "Response 1".into(),
                }],
                StopReason::Stop,
                None,
                dummy_metadata(),
            )
            .unwrap();

        session
            .add_user_input(TargetThread::Main, user_source(), "Second".into())
            .unwrap();

        // All 3 messages
        let history = session.chat_history(10);
        assert_eq!(history.len(), 3);

        // Only last 2
        let history = session.chat_history(2);
        assert_eq!(history.len(), 2);
        assert!(matches!(history[0].role, ChatHistoryRole::Assistant));
        assert!(matches!(history[1].role, ChatHistoryRole::User));
        assert!(
            matches!(&history[1].blocks[0], ChatHistoryBlock::Text { text } if text == "Second")
        );
    }

    #[test]
    fn chat_history_includes_tool_use_blocks() {
        use super::history::{ChatHistoryBlock, ChatHistoryRole};

        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "Use a tool".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        session
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

        let history = session.chat_history(10);
        assert_eq!(history.len(), 2);

        // Assistant message should have both text and tool_use blocks
        let assistant = &history[1];
        assert!(matches!(assistant.role, ChatHistoryRole::Assistant));
        assert_eq!(assistant.blocks.len(), 2);
        assert!(
            matches!(&assistant.blocks[0], ChatHistoryBlock::Text { text } if text == "Let me check.")
        );
        assert!(
            matches!(&assistant.blocks[1], ChatHistoryBlock::ToolUse { name, .. } if name == "get_weather")
        );
    }

    // ─── Thread graph tests ─────────────────────────────────────────────────

    #[test]
    fn thread_infos_returns_empty_when_no_thread() {
        let session = new_session();
        let infos = session.thread_infos();
        assert!(infos.is_empty());
    }

    #[test]
    fn thread_infos_returns_current_thread_after_prompt() {
        use super::history::{ThreadStartReasonKind, ThreadTurnState};

        let mut session = new_session();
        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();

        hydrate_threads(&mut session);

        let infos = session.thread_infos();
        assert_eq!(infos.len(), 1);
        assert!(infos[0].is_current);
        assert_eq!(infos[0].next_turn, ThreadTurnState::Assistant);
        assert_eq!(infos[0].start_reason, ThreadStartReasonKind::InitialThread);
    }

    #[test]
    fn thread_messages_resolves_conversation() {
        use super::history::ChatHistoryRole;

        let mut session = new_session();

        session
            .add_user_input(TargetThread::Main, user_source(), "Hello".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();
        let thread_id = session.current_main_thread.unwrap();
        hydrate_threads(&mut session);

        session
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

        session
            .add_user_input(TargetThread::Main, user_source(), "How are you?".into())
            .unwrap();
        let _ = session.next_prompt(TargetThread::Main).unwrap();

        let messages = session.thread_messages(thread_id).unwrap();

        // Should have 3 messages: user, assistant, user
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[0].role, ChatHistoryRole::User));
        assert!(matches!(messages[1].role, ChatHistoryRole::Assistant));
        assert!(matches!(messages[2].role, ChatHistoryRole::User));

        // Each message should carry its own block indexes
        assert!(!messages[0].block_indexes.is_empty());
        // First user message index should be 0
        assert_eq!(messages[0].block_indexes[0], 0);
    }

    #[test]
    fn thread_messages_returns_error_for_unknown_thread() {
        let session = new_session();
        let fake_id = SessionThreadId::new();
        let result = session.thread_messages(fake_id);
        assert!(matches!(result, Err(AgentSessionError::ThreadNotFound)));
    }
}
