//! Format-agnostic thread export types and builder.
//!
//! [`ExportableThread`] is a format-neutral intermediate representation that
//! entity.rs produces and format-specific modules (e.g. `pi_export`) consume.
//! This keeps the entity free of any particular export format's types.

use chrono::{DateTime, Utc};
use es_entity::EntityEvents;

use super::{
    entity::AgentSessionEvent,
    message::{AssistantBlock, StopReason, TargetThread},
    metadata::AssistantResponseMetadata,
    thread::SessionThreadId,
    AgentSessionId,
};

// ============================================================================
// Types
// ============================================================================

pub struct ExportableThread {
    pub session_id: AgentSessionId,
    pub model: String,
    pub base_timestamp: DateTime<Utc>,
    pub entries: Vec<ExportableEntry>,
}

pub enum ExportableEntry {
    UserMessage {
        text: String,
    },
    AssistantResponse {
        content: Vec<AssistantBlock>,
        stop_reason: StopReason,
        metadata: AssistantResponseMetadata,
    },
}

// ============================================================================
// Builder
// ============================================================================

pub(super) fn build_exportable_thread(
    session_id: AgentSessionId,
    model: &str,
    events: &EntityEvents<AgentSessionEvent>,
    thread_id: SessionThreadId,
    current_main_thread: Option<SessionThreadId>,
) -> ExportableThread {
    let base_timestamp = events.entity_first_persisted_at().unwrap_or_else(Utc::now);

    let mut entries = Vec::new();

    for event in events.iter_all() {
        match event {
            AgentSessionEvent::UserInputAdded { target, text, .. } => {
                if !targets_thread(target, thread_id, current_main_thread) {
                    continue;
                }
                entries.push(ExportableEntry::UserMessage { text: text.clone() });
            }
            // Skip SandboxNotificationAdded — they can fire between
            // tool_use and tool_result, breaking the API's required
            // assistant(tool_use) -> tool_result sequence.
            AgentSessionEvent::SandboxNotificationAdded { .. } => {}
            // Skip intermediate tool-use loops: only emit the final
            // assistant response per turn (non-ToolUse stop reason).
            AgentSessionEvent::AssistantResponseReceived {
                thread_id: tid,
                content,
                stop_reason,
                metadata,
                ..
            } if *tid == thread_id && !matches!(stop_reason, StopReason::ToolUse) => {
                entries.push(ExportableEntry::AssistantResponse {
                    content: content.clone(),
                    stop_reason: stop_reason.clone(),
                    metadata: metadata.clone(),
                });
            }
            // Skip tool results — they pair with tool_use assistant
            // messages which are also skipped above.
            AgentSessionEvent::ToolResultsAdded { .. } => {}
            _ => {}
        }
    }

    ExportableThread {
        session_id,
        model: model.to_string(),
        base_timestamp,
        entries,
    }
}

/// Whether a [`TargetThread`] references the given thread id.
fn targets_thread(
    target: &TargetThread,
    thread_id: SessionThreadId,
    current_main_thread: Option<SessionThreadId>,
) -> bool {
    match target {
        TargetThread::Main => current_main_thread == Some(thread_id),
        TargetThread::Id(id) => *id == thread_id,
    }
}
