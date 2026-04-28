//! Format-neutral IR produced by entity.rs and consumed by format-specific
//! exporters (e.g. `pi_export`).

use chrono::{DateTime, Utc};
use es_entity::EntityEvents;

use super::{
    entity::AgentSessionEvent,
    message::{AssistantBlock, StopReason, TargetThread},
    metadata::AssistantResponseMetadata,
    thread::SessionThreadId,
    AgentSessionId,
};

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
            // Skip: would break required assistant(tool_use) -> tool_result sequence.
            AgentSessionEvent::SandboxNotificationAdded { .. } => {}
            // Only emit the final assistant response per turn.
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
            // Skip: pair with tool_use assistant blocks which are also skipped.
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
