pub(super) mod estimation;
pub(super) mod prune;
pub(super) mod trigger;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use es_entity::EntityEvents;

use crate::agent::config::ModelDefaults;

use super::entity::{AgentSessionEvent, ThreadStartReason};
use super::message::{CompactionMetadata, SandboxOperation, TargetThread};
use super::settings::CompactionConfig;
use super::thread::{NewSessionThread, SessionThreadId};
use super::view::*;

use prune::{build_pruning_plan, PruningPlan};
use trigger::CompactionAction;

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

        AgentSessionEvent::CompactionApplied { from_thread_id, .. } => *from_thread_id == thread_id,

        AgentSessionEvent::Initialized { .. }
        | AgentSessionEvent::ToolDefsUpdated { .. }
        | AgentSessionEvent::SystemBlockUpdated { .. }
        | AgentSessionEvent::ModelDefaultsUpdated { .. }
        | AgentSessionEvent::ThreadStarted { .. }
        | AgentSessionEvent::ToolResultsMasked { .. }
        | AgentSessionEvent::OutputSubmitted { .. } => false,
    }
}

pub(super) struct CompactionResult {
    pub events: Vec<AgentSessionEvent>,
    pub new_thread: NewSessionThread,
    pub new_thread_id: SessionThreadId,
    pub prompt_definition: PromptDefinition,
}

/// Pure: returns `None` if no compaction is needed or the plan is empty.
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

    let plan = build_pruning_plan(
        events.iter_all(),
        current_thread_id,
        is_main_thread,
        config,
        current_prompt_definition,
    );
    if plan.is_empty() {
        return None;
    }

    let new_thread_id = SessionThreadId::new();

    // ToolResultsMasked is emitted before CompactionApplied, so its blocks
    // start at `current_block_count`.
    let current_block_count = count_blocks(events.iter_all());

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

    let prompt_definition = PromptDefinition {
        model: model_defaults.model.clone(),
        max_tokens_per_response: model_defaults.max_tokens_per_response,
        system_view: system_view.clone(),
        tool_definitions_view: tool_definitions_view.clone(),
        messages: transformed_messages.clone(),
        compaction: Some(compaction_metadata),
    };

    let new_thread = NewSessionThread::compacted(
        new_thread_id,
        session_id,
        model_defaults.clone(),
        system_view,
        tool_definitions_view,
        transformed_messages,
        current_thread_id,
    );

    let masked_indexes: Vec<MessageBlockIndex> = plan
        .masked_tool_results
        .iter()
        .map(|m| m.original_index)
        .collect();
    let cleared_thinking: Vec<MessageBlockIndex> = plan.cleared_thinking.into_iter().collect();
    let stripped_user_messages: Vec<MessageBlockIndex> =
        plan.stripped_user_messages.into_iter().collect();

    let mut session_events = Vec::new();

    // Emit masked tool results first so they participate in block indexing
    // and can be shared across threads.
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

/// `Duration::ZERO` if no assistant response has been persisted yet.
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

/// Brand-new thread with only pending user messages; preserves the active
/// sandbox notification (if any) as the first message.
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

    // Only carry forward user messages after the last PromptSent for this thread.
    let pending = pending_user_message_indexes(events.clone(), current_thread_id, is_main_thread);

    let sandbox_idx = last_active_sandbox_attach_index(events);
    let mut all_indexes = Vec::new();
    if let Some(idx) = sandbox_idx {
        all_indexes.push(idx);
    }
    all_indexes.extend(pending);

    let user_messages = UserMessagesView {
        indexes: all_indexes,
    };

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

/// Block indexes for user messages after the last `PromptSent`.
fn pending_user_message_indexes<'a>(
    events: impl DoubleEndedIterator<Item = &'a AgentSessionEvent> + Clone,
    thread_id: SessionThreadId,
    is_main_thread: bool,
) -> Vec<MessageBlockIndex> {
    let total_blocks = events.clone().fold(0usize, |acc, e| match e {
        AgentSessionEvent::UserInputAdded { .. }
        | AgentSessionEvent::SandboxNotificationAdded { .. } => acc + 1,
        AgentSessionEvent::AssistantResponseReceived { content, .. } => acc + content.len(),
        AgentSessionEvent::ToolResultsAdded { results, .. } => acc + results.len(),
        AgentSessionEvent::ToolResultsMasked { results, .. } => acc + results.len(),
        _ => acc,
    });

    // Scan backwards, collecting indexes until the last PromptSent.
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

/// Last sandbox attach not followed by a detach.
fn last_active_sandbox_attach_index<'a>(
    events: impl Iterator<Item = &'a AgentSessionEvent> + Clone,
) -> Option<MessageBlockIndex> {
    let mut block_counter = 0usize;
    let mut attached: HashMap<String, MessageBlockIndex> = HashMap::new();

    for event in events {
        match event {
            AgentSessionEvent::SandboxNotificationAdded {
                sandbox_name,
                operation,
                ..
            } => {
                let idx = MessageBlockIndex::new(block_counter);
                block_counter += 1;
                match operation {
                    SandboxOperation::Attach { .. } => {
                        attached.insert(sandbox_name.clone(), idx);
                    }
                    SandboxOperation::Detach => {
                        attached.remove(sandbox_name);
                    }
                }
            }
            AgentSessionEvent::UserInputAdded { .. } => {
                block_counter += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                block_counter += content.len();
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                block_counter += results.len();
            }
            AgentSessionEvent::ToolResultsMasked { results, .. } => {
                block_counter += results.len();
            }
            _ => {}
        }
    }

    // Typically at most one active sandbox.
    attached.into_values().max()
}

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

/// Filters stripped user messages and cleared thinking blocks; remaps
/// masked tool result indexes to their replacements.
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
