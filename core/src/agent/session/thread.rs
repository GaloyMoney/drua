use serde::{Deserialize, Serialize};

es_entity::entity_id! { SessionThreadId }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStartReason {
    InitialThread,
    TimeDeltaExceeded { previous_thread: SessionThreadId },
}

/// Lightweight thread view — NOT an EsEntity. Derived during session hydration
/// from `ThreadStarted` events and the messages that follow them.
#[derive(Debug, Clone)]
pub struct ThreadView {
    pub id: SessionThreadId,
    pub start_reason: ThreadStartReason,
    pub message_indices: Vec<usize>,
}
