use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use super::entity::ThreadStartReason;
use super::view::*;
use super::AgentSessionId;

es_entity::entity_id! { SessionThreadId }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextTurn {
    User,
    Assistant,
    ToolUse,
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
        max_tokens: u32,
        system_view: SystemView,
        tool_definitions_view: ToolDefinitionsView,
        initial_user_messages: UserMessagesView,
    },
    InitializedFromCompaction {
        id: SessionThreadId,
        session_id: AgentSessionId,
        model: String,
        max_tokens: u32,
        system_view: SystemView,
        tool_definitions_view: ToolDefinitionsView,
        messages: Vec<MessageView>,
        follows_from: SessionThreadId,
    },
    UserTurn {
        user_messages_view: UserMessagesView,
    },
    AssistantTurn {
        assistant_message_view: AssistantMessageView,
    },
    AssistantToolUse {
        assistant_message_view: AssistantMessageView,
    },
    ToolResultsTurn {
        tool_results_view: ToolResultsView,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct SessionThread {
    pub id: SessionThreadId,
    pub session_id: AgentSessionId,
    pub start_reason: ThreadStartReason,
    #[builder(default = "NextTurn::Assistant")]
    turn: NextTurn,
    events: EntityEvents<SessionThreadEvent>,
}

impl SessionThread {
    pub fn is_user_turn(&self) -> bool {
        self.turn == NextTurn::User
    }

    pub fn is_assistant_turn(&self) -> bool {
        self.turn == NextTurn::Assistant
    }

    pub fn is_tool_use_turn(&self) -> bool {
        self.turn == NextTurn::ToolUse
    }

    pub fn add_user_message(&mut self, user_messages_view: UserMessagesView) {
        self.events
            .push(SessionThreadEvent::UserTurn { user_messages_view });
        self.turn = NextTurn::Assistant;
    }

    pub fn add_assistant_message(&mut self, assistant_message_view: AssistantMessageView) {
        self.events.push(SessionThreadEvent::AssistantTurn {
            assistant_message_view,
        });
        self.turn = NextTurn::User;
    }

    pub fn add_assistant_tool_use(&mut self, assistant_message_view: AssistantMessageView) {
        self.events.push(SessionThreadEvent::AssistantToolUse {
            assistant_message_view,
        });
        self.turn = NextTurn::ToolUse;
    }

    pub fn add_tool_results(&mut self, tool_results_view: ToolResultsView) {
        self.events
            .push(SessionThreadEvent::ToolResultsTurn { tool_results_view });
        self.turn = NextTurn::Assistant;
    }

    pub fn prompt_definition(&self) -> PromptDefinition {
        let mut model = String::new();
        let mut max_tokens = 0;
        let mut system_view = SystemView { indexes: vec![] };
        let mut tool_definitions_view = ToolDefinitionsView { indexes: vec![] };
        let mut messages = Vec::new();

        for event in self.events.iter_all() {
            match event {
                SessionThreadEvent::Initialized {
                    model: m,
                    max_tokens: mt,
                    system_view: sv,
                    tool_definitions_view: tdv,
                    initial_user_messages,
                    ..
                } => {
                    model = m.clone();
                    max_tokens = *mt;
                    system_view = sv.clone();
                    tool_definitions_view = tdv.clone();
                    messages.push(MessageView::User(initial_user_messages.clone()));
                }
                SessionThreadEvent::InitializedFromCompaction {
                    model: m,
                    max_tokens: mt,
                    system_view: sv,
                    tool_definitions_view: tdv,
                    messages: init_messages,
                    ..
                } => {
                    model = m.clone();
                    max_tokens = *mt;
                    system_view = sv.clone();
                    tool_definitions_view = tdv.clone();
                    messages.extend(init_messages.iter().cloned());
                }
                SessionThreadEvent::UserTurn {
                    user_messages_view, ..
                } => {
                    messages.push(MessageView::User(user_messages_view.clone()));
                }
                SessionThreadEvent::AssistantTurn {
                    assistant_message_view,
                    ..
                }
                | SessionThreadEvent::AssistantToolUse {
                    assistant_message_view,
                    ..
                } => {
                    messages.push(MessageView::Assistant(assistant_message_view.clone()));
                }
                SessionThreadEvent::ToolResultsTurn {
                    tool_results_view, ..
                } => {
                    messages.push(MessageView::ToolResults(tool_results_view.clone()));
                }
            }
        }

        PromptDefinition {
            model,
            max_tokens,
            system_view,
            tool_definitions_view,
            messages,
            compaction: None,
        }
    }
}

impl TryFromEvents<SessionThreadEvent> for SessionThread {
    fn try_from_events(
        events: EntityEvents<SessionThreadEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = SessionThreadBuilder::default();

        for event in events.iter_all() {
            match event {
                SessionThreadEvent::Initialized {
                    id,
                    session_id,
                    start_reason,
                    ..
                } => {
                    builder = builder
                        .id(*id)
                        .session_id(*session_id)
                        .start_reason(*start_reason);
                }
                SessionThreadEvent::InitializedFromCompaction {
                    id,
                    session_id,
                    messages,
                    ..
                } => {
                    builder = builder.id(*id).session_id(*session_id);
                    // Determine turn from last message in compacted history
                    let turn = match messages.last() {
                        Some(MessageView::User(_)) => NextTurn::Assistant,
                        Some(MessageView::Assistant(_)) => NextTurn::User,
                        Some(MessageView::ToolResults(_)) => NextTurn::Assistant,
                        None => NextTurn::User,
                    };
                    builder = builder.turn(turn);
                }
                SessionThreadEvent::UserTurn { .. } => {
                    builder = builder.turn(NextTurn::Assistant);
                }
                SessionThreadEvent::AssistantTurn { .. } => {
                    builder = builder.turn(NextTurn::User);
                }
                SessionThreadEvent::AssistantToolUse { .. } => {
                    builder = builder.turn(NextTurn::ToolUse);
                }
                SessionThreadEvent::ToolResultsTurn { .. } => {
                    builder = builder.turn(NextTurn::Assistant);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug)]
pub struct NewSessionThread {
    pub(super) id: SessionThreadId,
    pub(super) session_id: AgentSessionId,
    pub(super) model: String,
    pub(super) max_tokens: u32,
    pub(super) system_view: SystemView,
    pub(super) tool_definitions_view: ToolDefinitionsView,
    init: ThreadInitData,
}

#[derive(Debug)]
enum ThreadInitData {
    Initial {
        initial_user_messages: UserMessagesView,
    },
    Compacted {
        messages: Vec<MessageView>,
        follows_from: SessionThreadId,
    },
}

impl NewSessionThread {
    pub fn builder() -> NewSessionThreadBuilder {
        NewSessionThreadBuilder {
            id: SessionThreadId::new(),
            session_id: None,
            model: None,
            max_tokens: None,
            system_view: None,
            tool_definitions_view: None,
            initial_user_messages: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn compacted(
        id: SessionThreadId,
        session_id: AgentSessionId,
        model: String,
        max_tokens: u32,
        system_view: SystemView,
        tool_definitions_view: ToolDefinitionsView,
        messages: Vec<MessageView>,
        follows_from: SessionThreadId,
    ) -> Self {
        Self {
            id,
            session_id,
            model,
            max_tokens,
            system_view,
            tool_definitions_view,
            init: ThreadInitData::Compacted {
                messages,
                follows_from,
            },
        }
    }
}

/// Builder for the initial thread case (preserves existing API).
pub struct NewSessionThreadBuilder {
    id: SessionThreadId,
    session_id: Option<AgentSessionId>,
    model: Option<String>,
    max_tokens: Option<u32>,
    system_view: Option<SystemView>,
    tool_definitions_view: Option<ToolDefinitionsView>,
    initial_user_messages: Option<UserMessagesView>,
}

impl NewSessionThreadBuilder {
    pub fn id(mut self, id: SessionThreadId) -> Self {
        self.id = id;
        self
    }

    pub fn session_id(mut self, session_id: AgentSessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn system_view(mut self, system_view: SystemView) -> Self {
        self.system_view = Some(system_view);
        self
    }

    pub fn tool_definitions_view(mut self, tool_definitions_view: ToolDefinitionsView) -> Self {
        self.tool_definitions_view = Some(tool_definitions_view);
        self
    }

    pub fn initial_user_messages(mut self, initial_user_messages: UserMessagesView) -> Self {
        self.initial_user_messages = Some(initial_user_messages);
        self
    }

    pub fn build(self) -> Result<NewSessionThread, EntityHydrationError> {
        Ok(NewSessionThread {
            id: self.id,
            session_id: self.session_id.ok_or(EntityHydrationError::from(
                derive_builder::UninitializedFieldError::from("session_id"),
            ))?,
            model: self.model.ok_or(EntityHydrationError::from(
                derive_builder::UninitializedFieldError::from("model"),
            ))?,
            max_tokens: self.max_tokens.ok_or(EntityHydrationError::from(
                derive_builder::UninitializedFieldError::from("max_tokens"),
            ))?,
            system_view: self.system_view.ok_or(EntityHydrationError::from(
                derive_builder::UninitializedFieldError::from("system_view"),
            ))?,
            tool_definitions_view: self
                .tool_definitions_view
                .ok_or(EntityHydrationError::from(
                    derive_builder::UninitializedFieldError::from("tool_definitions_view"),
                ))?,
            init: ThreadInitData::Initial {
                initial_user_messages: self.initial_user_messages.ok_or(
                    EntityHydrationError::from(derive_builder::UninitializedFieldError::from(
                        "initial_user_messages",
                    )),
                )?,
            },
        })
    }
}

impl IntoEvents<SessionThreadEvent> for NewSessionThread {
    fn into_events(self) -> EntityEvents<SessionThreadEvent> {
        let event = match self.init {
            ThreadInitData::Initial {
                initial_user_messages,
            } => SessionThreadEvent::Initialized {
                id: self.id,
                session_id: self.session_id,
                start_reason: self.start_reason,
                model: self.model,
                max_tokens: self.max_tokens,
                system_view: self.system_view,
                tool_definitions_view: self.tool_definitions_view,
                initial_user_messages,
            },
            ThreadInitData::Compacted {
                messages,
                follows_from,
            } => SessionThreadEvent::InitializedFromCompaction {
                id: self.id,
                session_id: self.session_id,
                model: self.model,
                max_tokens: self.max_tokens,
                system_view: self.system_view,
                tool_definitions_view: self.tool_definitions_view,
                messages,
                follows_from,
            },
        };
        EntityEvents::init(self.id, [event])
    }
}
