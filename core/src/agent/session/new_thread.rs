use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::UserMessageSource;
use es_entity::*;
use llm::prompt::{AssistantBlock, SystemBlock, Tool};

use super::{error::AgentSessionError, AgentSessionId};

es_entity::entity_id! { SessionThreadId }

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SessionThreadId")]
pub enum SessionThreadEvent {
    Initialized {
        id: SessionThreadId,
        session_id: AgentSessionId,
    },
}
