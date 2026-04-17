use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

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
        model: String,
        max_tokens: u32,
        system_view: SystemView,
        tool_definitions_view: ToolDefinitionsView,
        initial_user_messages: UserMessagesView,
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
                SessionThreadEvent::Initialized { id, session_id, .. } => {
                    builder = builder.id(*id).session_id(*session_id);
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

#[derive(Debug, Builder)]
pub struct NewSessionThread {
    #[builder(setter(into))]
    pub(super) id: SessionThreadId,
    pub(super) session_id: AgentSessionId,
    #[builder(setter(into))]
    pub(super) model: String,
    pub(super) max_tokens: u32,
    pub(super) system_view: SystemView,
    pub(super) tool_definitions_view: ToolDefinitionsView,
    pub(super) initial_user_messages: UserMessagesView,
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
                model: self.model,
                max_tokens: self.max_tokens,
                system_view: self.system_view,
                tool_definitions_view: self.tool_definitions_view,
                initial_user_messages: self.initial_user_messages,
            }],
        )
    }
}
