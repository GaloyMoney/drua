pub(super) mod estimation;
pub(super) mod prune;
pub(super) mod trigger;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use es_entity::EntityEvents;

use crate::agent::config::ModelDefaults;

use super::entity::{AgentSessionEvent, ThreadStartReason};
use super::message::{CompactionMetadata, TargetThread};
use super::settings::CompactionConfig;
use super::thread::{NewSessionThread, SessionThreadId};
use super::view::*;

use prune::{build_pruning_plan, PruningPlan};
use trigger::CompactionAction;

/// Returns true if the given event belongs to (was produced on) the specified thread.
///
/// Events with an explicit `thread_id` field match directly.
/// Events with a `target: TargetThread` field match if:
/// - `TargetThread::Main` — matches only when `is_main_thread` is true
/// - `TargetThread::Id(id)` — matches when `id == thread_id`
///
/// Global events (Initialized, ToolDefsUpdated, etc.) return `false`.
pub(super) fn event_belongs_to_thread(
    event: &AgentSessionEvent,
    thread_id: SessionThreadId,
    is_main_thread: bool,
) -> bool {
    match event {
        AgentSessionEvent::AssistantResponseReceived { thread_id: tid, .. }
        | AgentSessionEvent::ToolResultsAdded { thread_id: tid, .. }
        | AgentSessionEvent::PromptSent { thread_id: tid, .. } => *tid == thread_id,

        AgentSessionEvent::UserInputAdded { target, .. }
        | AgentSessionEvent::SandboxNotificationAdded { target, .. } => match target {
            TargetThread::Main => is_main_thread,
            TargetThread::Id(id) => *id == thread_id,
        },

        // CompactionApplied belongs to the source thread
        AgentSessionEvent::CompactionApplied { from_thread_id, .. } => *from_thread_id == thread_id,

        // Global events don't belong to any specific thread
        AgentSessionEvent::Initialized { .. }
        | AgentSessionEvent::ToolDefsUpdated { .. }
        | AgentSessionEvent::SystemBlocksUpdated { .. }
        | AgentSessionEvent::ThreadStarted { .. }
        | AgentSessionEvent::ToolResultsMasked { .. } => false,
    }
}

/// The result of applying compaction to a session.
pub(super) struct CompactionResult {
    /// Events to push to the session entity.
    pub events: Vec<AgentSessionEvent>,
    /// New compacted thread entity to add.
    pub new_thread: NewSessionThread,
    /// The new thread's id (for tracking).
    pub new_thread_id: SessionThreadId,
    /// The prompt definition built for the new compacted thread (with metadata).
    pub prompt_definition: PromptDefinition,
}

/// Attempt pruning compaction. Returns `None` if no compaction is needed or
/// the pruning plan is empty.
///
/// This function is pure: it reads the event stream and config, builds a
/// pruning plan, and returns the events + new thread that the caller must
/// apply to the session entity.
#[allow(clippy::too_many_arguments)]
pub(super) fn maybe_prune(
    events: &EntityEvents<AgentSessionEvent>,
    config: &CompactionConfig,
    model_defaults: &ModelDefaults,
    session_id: super::AgentSessionId,
    current_thread_id: SessionThreadId,
    is_main_thread: bool,
    current_prompt_definition: &PromptDefinition,
) -> Option<CompactionResult> {
    let last_usage = estimation::last_usage(events.iter_all(), current_thread_id);
    let estimated_tokens = estimation::estimate_context_tokens(
        events.iter_all(),
        current_thread_id,
        is_main_thread,
        last_usage,
    );

    let time_since_last_turn = time_since_last_response_on_thread(events, current_thread_id);

    let action = config.determine_action(
        estimated_tokens,
        model_defaults.context_window_tokens,
        time_since_last_turn,
    );

    match action {
        CompactionAction::None => return None,
        CompactionAction::Orphan => {
            return build_orphan_result(
                events.iter_all(),
                session_id,
                current_thread_id,
                is_main_thread,
                model_defaults,
                current_prompt_definition,
            );
        }
        CompactionAction::PruneOpportunistic | CompactionAction::PruneThenSummarize => {}
    }

    let plan = build_pruning_plan(events.iter_all(), current_thread_id, is_main_thread, config);
    if plan.is_empty() {
        return None;
    }

    let new_thread_id = SessionThreadId::new();

    // Compute the current total block count so we know where masked
    // replacement blocks will land in the global content list.
    let current_block_count = count_blocks(events.iter_all());

    // Build original→replacement index mapping for masked tool results.
    // The ToolResultsMasked event is emitted first (before CompactionApplied),
    // so its blocks start at `current_block_count`.
    let mask_remap: HashMap<MessageBlockIndex, MessageBlockIndex> = plan
        .masked_tool_results
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                m.original_index,
                MessageBlockIndex::new(current_block_count + i),
            )
        })
        .collect();

    // Transform the current thread's message views to apply the pruning plan.
    // This remaps masked tool result indexes so that `into_prompt()` resolves
    // to the masked replacement content.
    let transformed_messages =
        transform_message_views(&current_prompt_definition.messages, &plan, &mask_remap);

    let compaction_metadata = CompactionMetadata {
        follows_from: current_thread_id,
        tool_results_masked: plan.masked_tool_results.len(),
        thinking_blocks_cleared: plan.cleared_thinking.len(),
        sandbox_notifications_stripped: plan.stripped_user_messages.len(),
        estimated_tokens_saved: plan.estimated_tokens_saved,
    };

    let system_view = current_prompt_definition.system_view().clone();
    let tool_definitions_view = current_prompt_definition.tool_definitions_view().clone();

    // Build the PromptDefinition for the compacted thread
    let prompt_definition = PromptDefinition {
        model: model_defaults.model.clone(),
        max_tokens_per_response: model_defaults.max_tokens_per_response,
        system_view: system_view.clone(),
        tool_definitions_view: tool_definitions_view.clone(),
        messages: transformed_messages.clone(),
        compaction: Some(compaction_metadata),
    };

    // Build the new compacted thread
    let new_thread = NewSessionThread::compacted(
        new_thread_id,
        session_id,
        model_defaults.clone(),
        system_view,
        tool_definitions_view,
        transformed_messages,
        current_thread_id,
    );

    // Build session-level events
    let masked_indexes: Vec<MessageBlockIndex> = plan
        .masked_tool_results
        .iter()
        .map(|m| m.original_index)
        .collect();
    let cleared_thinking: Vec<MessageBlockIndex> = plan.cleared_thinking.into_iter().collect();
    let stripped_user_messages: Vec<MessageBlockIndex> =
        plan.stripped_user_messages.into_iter().collect();

    let mut session_events = Vec::new();

    // Emit masked tool results into the main event stream first so they
    // participate in block indexing and can be shared across threads.
    if !plan.masked_tool_results.is_empty() {
        session_events.push(AgentSessionEvent::ToolResultsMasked {
            results: plan.masked_tool_results,
        });
    }

    session_events.push(AgentSessionEvent::CompactionApplied {
        from_thread_id: current_thread_id,
        new_thread_id,
        masked_tool_results: masked_indexes,
        cleared_thinking,
        stripped_user_messages,
        estimated_tokens_saved: plan.estimated_tokens_saved,
    });
    session_events.push(AgentSessionEvent::ThreadStarted {
        thread_id: new_thread_id,
        start_reason: ThreadStartReason::Compaction {
            from_thread: current_thread_id,
        },
    });

    Some(CompactionResult {
        events: session_events,
        new_thread,
        new_thread_id,
        prompt_definition,
    })
}

/// Compute duration since the last assistant response on a specific thread.
/// Falls back to Duration::ZERO when no assistant response has been persisted yet.
fn time_since_last_response_on_thread(
    events: &EntityEvents<AgentSessionEvent>,
    thread_id: SessionThreadId,
) -> Duration {
    let last_ts = events
        .iter_persisted()
        .rev()
        .find_map(|pe| match &pe.event {
            AgentSessionEvent::AssistantResponseReceived { thread_id: tid, .. }
                if *tid == thread_id =>
            {
                Some(pe.recorded_at)
            }
            _ => None,
        });
    match last_ts {
        Some(ts) => {
            let elapsed = chrono::Utc::now() - ts;
            elapsed.to_std().unwrap_or(Duration::ZERO)
        }
        None => Duration::ZERO,
    }
}

/// Build an orphan result: a brand-new thread with only the pending user
/// messages (no conversation carry-over). Used when idle time exceeds the
/// configured reset threshold.
fn build_orphan_result<'a>(
    events: impl DoubleEndedIterator<Item = &'a AgentSessionEvent> + Clone,
    session_id: super::AgentSessionId,
    current_thread_id: SessionThreadId,
    is_main_thread: bool,
    model_defaults: &ModelDefaults,
    current_prompt_definition: &PromptDefinition,
) -> Option<CompactionResult> {
    let new_thread_id = SessionThreadId::new();
    let system_view = current_prompt_definition.system_view().clone();
    let tool_definitions_view = current_prompt_definition.tool_definitions_view().clone();

    // Only carry forward user messages that arrived after the last PromptSent
    // for this thread — not the entire conversation history.
    let pending = pending_user_message_indexes(events, current_thread_id, is_main_thread);
    let user_messages = UserMessagesView { indexes: pending };

    let prompt_definition = PromptDefinition {
        model: model_defaults.model.clone(),
        max_tokens_per_response: model_defaults.max_tokens_per_response,
        system_view: system_view.clone(),
        tool_definitions_view: tool_definitions_view.clone(),
        messages: vec![MessageView::User(user_messages.clone())],
        compaction: None,
    };

    let session_events = vec![AgentSessionEvent::ThreadStarted {
        thread_id: new_thread_id,
        start_reason: ThreadStartReason::Orphan {
            from_thread: current_thread_id,
        },
    }];

    Some(CompactionResult {
        events: session_events,
        new_thread: NewSessionThread::orphaned(
            new_thread_id,
            session_id,
            current_thread_id,
            model_defaults.clone(),
            system_view,
            tool_definitions_view,
            user_messages,
        ),
        new_thread_id,
        prompt_definition,
    })
}

/// Collect block indexes for user messages that arrived after the last
/// `PromptSent` for the given thread. These are the "pending" inputs that
/// should carry over to an orphan thread.
fn pending_user_message_indexes<'a>(
    events: impl DoubleEndedIterator<Item = &'a AgentSessionEvent> + Clone,
    thread_id: SessionThreadId,
    is_main_thread: bool,
) -> Vec<MessageBlockIndex> {
    // First pass: count total blocks across all events
    let total_blocks = events.clone().fold(0usize, |acc, e| match e {
        AgentSessionEvent::UserInputAdded { .. }
        | AgentSessionEvent::SandboxNotificationAdded { .. } => acc + 1,
        AgentSessionEvent::AssistantResponseReceived { content, .. } => acc + content.len(),
        AgentSessionEvent::ToolResultsAdded { results, .. } => acc + results.len(),
        AgentSessionEvent::ToolResultsMasked { results, .. } => acc + results.len(),
        _ => acc,
    });

    // Second pass: scan backwards, collecting indexes until we hit the last
    // PromptSent for this thread.
    let mut block_counter = total_blocks;
    let mut pending = Vec::new();
    for event in events.rev() {
        match event {
            AgentSessionEvent::PromptSent { thread_id: tid, .. } if *tid == thread_id => break,
            AgentSessionEvent::UserInputAdded { target, .. }
            | AgentSessionEvent::SandboxNotificationAdded { target, .. } => {
                block_counter -= 1;
                let targets_this = match target {
                    TargetThread::Main => is_main_thread,
                    TargetThread::Id(id) => *id == thread_id,
                };
                if targets_this {
                    pending.push(MessageBlockIndex::new(block_counter));
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
    pending.reverse();
    pending
}

/// Count the total number of content blocks in the event stream.
fn count_blocks<'a>(events: impl Iterator<Item = &'a AgentSessionEvent>) -> usize {
    events.fold(0usize, |acc, e| match e {
        AgentSessionEvent::UserInputAdded { .. }
        | AgentSessionEvent::SandboxNotificationAdded { .. } => acc + 1,
        AgentSessionEvent::AssistantResponseReceived { content, .. } => acc + content.len(),
        AgentSessionEvent::ToolResultsAdded { results, .. } => acc + results.len(),
        AgentSessionEvent::ToolResultsMasked { results, .. } => acc + results.len(),
        _ => acc,
    })
}

/// Transform message views by applying the pruning plan:
/// - Filter out stripped user messages from User views
/// - Filter out cleared thinking blocks from Assistant views
/// - Remap masked tool result indexes to their replacement indexes
fn transform_message_views(
    messages: &[MessageView],
    plan: &PruningPlan,
    mask_remap: &HashMap<MessageBlockIndex, MessageBlockIndex>,
) -> Vec<MessageView> {
    let stripped: HashSet<&MessageBlockIndex> = plan.stripped_user_messages.iter().collect();
    let cleared: HashSet<&MessageBlockIndex> = plan.cleared_thinking.iter().collect();

    let mut result = Vec::with_capacity(messages.len());

    for msg in messages {
        match msg {
            MessageView::User(user_view) => {
                let filtered: Vec<MessageBlockIndex> = user_view
                    .indexes
                    .iter()
                    .filter(|idx| !stripped.contains(idx))
                    .copied()
                    .collect();
                if !filtered.is_empty() {
                    result.push(MessageView::User(UserMessagesView { indexes: filtered }));
                }
            }
            MessageView::Assistant(assistant_view) => {
                let filtered: Vec<MessageBlockIndex> = assistant_view
                    .indexes
                    .iter()
                    .filter(|idx| !cleared.contains(idx))
                    .copied()
                    .collect();
                if !filtered.is_empty() {
                    result.push(MessageView::Assistant(AssistantMessageView {
                        indexes: filtered,
                    }));
                }
            }
            MessageView::ToolResults(tool_results_view) => {
                // Remap masked tool result indexes to their replacement indexes.
                let remapped: Vec<MessageBlockIndex> = tool_results_view
                    .indexes
                    .iter()
                    .map(|idx| mask_remap.get(idx).copied().unwrap_or(*idx))
                    .collect();
                result.push(MessageView::ToolResults(ToolResultsView {
                    indexes: remapped,
                }));
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_strips_user_messages() {
        let messages = vec![
            MessageView::User(UserMessagesView {
                indexes: vec![
                    MessageBlockIndex::new(0),
                    MessageBlockIndex::new(1),
                    MessageBlockIndex::new(2),
                ],
            }),
            MessageView::Assistant(AssistantMessageView {
                indexes: vec![MessageBlockIndex::new(0), MessageBlockIndex::new(1)],
            }),
        ];

        let plan = PruningPlan {
            stripped_user_messages: [MessageBlockIndex::new(1)].into_iter().collect(),
            ..Default::default()
        };

        let result = transform_message_views(&messages, &plan, &HashMap::new());
        assert_eq!(result.len(), 2);
        match &result[0] {
            MessageView::User(v) => assert_eq!(v.indexes.len(), 2),
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn transform_clears_thinking_blocks() {
        let messages = vec![MessageView::Assistant(AssistantMessageView {
            indexes: vec![
                MessageBlockIndex::new(0),
                MessageBlockIndex::new(1),
                MessageBlockIndex::new(2),
            ],
        })];

        let plan = PruningPlan {
            cleared_thinking: [MessageBlockIndex::new(0), MessageBlockIndex::new(1)]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let result = transform_message_views(&messages, &plan, &HashMap::new());
        assert_eq!(result.len(), 1);
        match &result[0] {
            MessageView::Assistant(v) => {
                assert_eq!(v.indexes.len(), 1);
                assert_eq!(v.indexes[0], MessageBlockIndex::new(2));
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn transform_drops_empty_views() {
        let messages = vec![
            MessageView::User(UserMessagesView {
                indexes: vec![MessageBlockIndex::new(0)],
            }),
            MessageView::Assistant(AssistantMessageView {
                indexes: vec![MessageBlockIndex::new(0)],
            }),
        ];

        let plan = PruningPlan {
            stripped_user_messages: [MessageBlockIndex::new(0)].into_iter().collect(),
            cleared_thinking: [MessageBlockIndex::new(0)].into_iter().collect(),
            ..Default::default()
        };

        let result = transform_message_views(&messages, &plan, &HashMap::new());
        assert!(result.is_empty());
    }

}
