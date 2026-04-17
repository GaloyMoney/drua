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
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SessionThreadId")]
pub enum SessionThreadEvent {
    Initialized {
        id: SessionThreadId,
        session_id: AgentSessionId,
        model: String,
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

    pub fn add_user_message(&mut self, user_messages_view: UserMessagesView) {
        self.events.push(SessionThreadEvent::UserTurn {
            user_messages_view,
        });
        self.turn = NextTurn::Assistant;
    }

    pub fn add_assistant_message(&mut self, assistant_message_view: AssistantMessageView) {
        self.events.push(SessionThreadEvent::AssistantTurn {
            assistant_message_view,
        });
        self.turn = NextTurn::User;
    }

    pub fn prompt_definition(&self) -> PromptDefinition {
        let mut model = String::new();
        let mut system_view = SystemView { indexes: vec![] };
        let mut tool_definitions_view = ToolDefinitionsView { indexes: vec![] };
        let mut messages = Vec::new();

        for event in self.events.iter_all() {
            match event {
                SessionThreadEvent::Initialized {
                    model: m,
                    system_view: sv,
                    tool_definitions_view: tdv,
                    initial_user_messages,
                    ..
                } => {
                    model = m.clone();
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
                } => {
                    messages.push(MessageView::Assistant(assistant_message_view.clone()));
                }
            }
        }

        PromptDefinition {
            model,
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
                system_view: self.system_view,
                tool_definitions_view: self.tool_definitions_view,
                initial_user_messages: self.initial_user_messages,
            }],
        )
    }
}
