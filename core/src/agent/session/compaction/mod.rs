pub(super) mod estimation;
pub(super) mod prune;
pub(super) mod trigger;

use std::collections::HashSet;
use std::time::Duration;

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
        | AgentSessionEvent::ThreadStarted { .. } => false,
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
pub(super) fn maybe_prune<'a>(
    events: impl DoubleEndedIterator<Item = &'a AgentSessionEvent> + Clone,
    config: &CompactionConfig,
    session_id: super::AgentSessionId,
    current_thread_id: SessionThreadId,
    is_main_thread: bool,
    current_prompt_definition: &PromptDefinition,
    time_since_last_turn: Duration,
) -> Option<CompactionResult> {
    let last_usage = estimation::last_usage(events.clone(), current_thread_id);
    let estimated_tokens = estimation::estimate_context_tokens(
        events.clone(),
        current_thread_id,
        is_main_thread,
        last_usage,
    );

    let action = config.determine_action(estimated_tokens, time_since_last_turn);

    match action {
        CompactionAction::None => return None,
        CompactionAction::PruneOpportunistic | CompactionAction::PruneThenSummarize => {}
    }

    let plan = build_pruning_plan(events, current_thread_id, is_main_thread, config);
    if plan.is_empty() {
        return None;
    }

    let new_thread_id = SessionThreadId::new();

    // Transform the current thread's message views to apply the pruning plan
    let transformed_messages = transform_message_views(&current_prompt_definition.messages, &plan);

    let compaction_metadata = CompactionMetadata {
        follows_from: current_thread_id,
        tool_results_masked: plan.masked_tool_results.len(),
        thinking_blocks_cleared: plan.cleared_thinking.len(),
        sandbox_notifications_stripped: plan.stripped_user_messages.len(),
        estimated_tokens_saved: plan.estimated_tokens_saved,
    };

    let system_view = current_prompt_definition.system_view().clone();
    let tool_definitions_view = current_prompt_definition.tool_definitions_view().clone();
    let model = current_prompt_definition.model.clone();
    let max_tokens = current_prompt_definition.max_tokens;

    // Build the PromptDefinition for the compacted thread
    let prompt_definition = PromptDefinition {
        model: model.clone(),
        max_tokens,
        system_view: system_view.clone(),
        tool_definitions_view: tool_definitions_view.clone(),
        messages: transformed_messages.clone(),
        compaction: Some(compaction_metadata),
    };

    // Build the new compacted thread
    let new_thread = NewSessionThread::compacted(
        new_thread_id,
        session_id,
        model,
        max_tokens,
        system_view,
        tool_definitions_view,
        transformed_messages,
        current_thread_id,
    );

    // Build session-level events
    let cleared_thinking: Vec<MessageBlockIndex> = plan.cleared_thinking.into_iter().collect();
    let stripped_user_messages: Vec<MessageBlockIndex> =
        plan.stripped_user_messages.into_iter().collect();

    let session_events = vec![
        AgentSessionEvent::CompactionApplied {
            from_thread_id: current_thread_id,
            new_thread_id,
            masked_tool_results: plan.masked_tool_results,
            cleared_thinking,
            stripped_user_messages,
            estimated_tokens_saved: plan.estimated_tokens_saved,
        },
        AgentSessionEvent::ThreadStarted {
            thread_id: new_thread_id,
            start_reason: ThreadStartReason::Compaction {
                from_thread: current_thread_id,
            },
        },
    ];

    Some(CompactionResult {
        events: session_events,
        new_thread,
        new_thread_id,
        prompt_definition,
    })
}

/// Transform message views by applying the pruning plan:
/// - Filter out stripped user messages from User views
/// - Filter out cleared thinking blocks from Assistant views
///
/// Tool result masking does NOT need view transformation here — the masked
/// tool results are persisted via CompactionApplied and get new natural
/// indexes when into_prompt() walks the event stream.
fn transform_message_views(messages: &[MessageView], plan: &PruningPlan) -> Vec<MessageView> {
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
            MessageView::ToolResults(_) => {
                // Tool results views pass through unchanged — masking is handled
                // by the CompactionApplied event giving replacements new indexes
                result.push(msg.clone());
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

        let result = transform_message_views(&messages, &plan);
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

        let result = transform_message_views(&messages, &plan);
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

        let result = transform_message_views(&messages, &plan);
        assert!(result.is_empty());
    }
}
