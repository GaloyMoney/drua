use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use super::AgentSessionId;

es_entity::entity_id! { SessionThreadId }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextTurn {
    User,
    Agent,
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SessionThreadId")]
pub enum SessionThreadEvent {
    Initialized {
        id: SessionThreadId,
        session_id: AgentSessionId,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct SessionThread {
    pub id: SessionThreadId,
    pub session_id: AgentSessionId,
    #[builder(default = "NextTurn::User")]
    turn: NextTurn,
    events: EntityEvents<SessionThreadEvent>,
}

impl SessionThread {
    pub fn is_user_turn(&self) -> bool {
        self.turn == NextTurn::User
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
                    id, session_id, ..
                } => {
                    builder = builder.id(*id).session_id(*session_id);
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
            }],
        )
    }
}
