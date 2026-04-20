use std::collections::{HashMap, HashSet};

use super::super::entity::{AgentSessionEvent, MaskedToolResult};
use super::super::message::{AssistantBlock, SandboxOperation, ToolResultInput};
use super::super::settings::CompactionConfig;
use super::super::view::MessageBlockIndex;
use super::estimation;

const MASK_PLACEHOLDER: &str = "[Tool output cleared — re-invoke tool if needed]";

/// The result of analysing a session's event stream for pruning opportunities.
#[derive(Debug, Default)]
pub struct PruningPlan {
    /// Tool results to mask: carries the replacement content with placeholder text.
    pub masked_tool_results: Vec<MaskedToolResult>,
    /// Thinking block indexes to exclude from new thread's assistant views.
    pub cleared_thinking: HashSet<MessageBlockIndex>,
    /// User message indexes (sandbox notifications) to strip from new thread's views.
    pub stripped_user_messages: HashSet<MessageBlockIndex>,
    /// Estimated tokens saved by applying this plan.
    pub estimated_tokens_saved: u64,
}

impl PruningPlan {
    pub fn is_empty(&self) -> bool {
        self.masked_tool_results.is_empty()
            && self.cleared_thinking.is_empty()
            && self.stripped_user_messages.is_empty()
    }
}

/// Build a complete pruning plan by combining all three pruning operations.
pub fn build_pruning_plan(events: &[AgentSessionEvent], config: &CompactionConfig) -> PruningPlan {
    let sandbox_tracker = SandboxTracker::from_events(events);

    let (masked, mask_tokens) =
        plan_tool_result_masking(events, config.keep_recent_tool_results, &sandbox_tracker);
    let (cleared, think_tokens) = plan_thinking_clearing(events);
    let (stripped, sandbox_tokens) = plan_sandbox_stripping(events);

    PruningPlan {
        masked_tool_results: masked,
        cleared_thinking: cleared,
        stripped_user_messages: stripped,
        estimated_tokens_saved: mask_tokens + think_tokens + sandbox_tokens,
    }
}

// ============================================================================
// Tool result masking
// ============================================================================

/// Identify tool results to mask. Keeps the `keep_recent` most recent intact,
/// masks the rest with a placeholder that preserves the tool name and sandbox
/// context.
fn plan_tool_result_masking(
    events: &[AgentSessionEvent],
    keep_recent: usize,
    sandbox_tracker: &SandboxTracker,
) -> (Vec<MaskedToolResult>, u64) {
    // Collect all tool results with their unified block indexes and event positions
    let mut all_results: Vec<(MessageBlockIndex, usize, &ToolResultInput)> = Vec::new();
    let mut block_idx = 0usize;

    for (event_idx, event) in events.iter().enumerate() {
        match event {
            AgentSessionEvent::UserInputAdded { .. }
            | AgentSessionEvent::SandboxNotificationAdded { .. } => {
                block_idx += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                block_idx += content.len();
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                for result in results {
                    all_results.push((MessageBlockIndex::new(block_idx), event_idx, result));
                    block_idx += 1;
                }
            }
            AgentSessionEvent::CompactionApplied {
                masked_tool_results,
                ..
            } => {
                // CompactionApplied adds replacement entries to the block array
                block_idx += masked_tool_results.len();
            }
            _ => {}
        }
    }

    if all_results.len() <= keep_recent {
        return (Vec::new(), 0);
    }

    let mask_count = all_results.len() - keep_recent;
    let to_mask = &all_results[..mask_count];

    let mut masked = Vec::with_capacity(mask_count);
    let mut tokens_saved = 0u64;

    for (idx, event_idx, result) in to_mask {
        // Skip results that are already tiny (previously masked)
        let content_len = result.content.len();
        if content_len <= MASK_PLACEHOLDER.len() + 50 {
            continue;
        }

        let sandbox_prefix = sandbox_tracker
            .sandbox_at(*event_idx)
            .map(|s| format!("[sandbox:{s}] "))
            .unwrap_or_default();

        let masked_content = format!("{sandbox_prefix}{MASK_PLACEHOLDER}");
        let tokens_before = estimation::estimate_event_tokens_for_content(content_len);
        let tokens_after = estimation::estimate_event_tokens_for_content(masked_content.len());
        tokens_saved += tokens_before.saturating_sub(tokens_after);

        masked.push(MaskedToolResult {
            original_index: *idx,
            replacement: ToolResultInput {
                tool_use_id: result.tool_use_id.clone(),
                content: masked_content,
                is_error: result.is_error,
            },
        });
    }

    (masked, tokens_saved)
}

// ============================================================================
// Thinking block clearing
// ============================================================================

/// Identify assistant responses whose thinking blocks should be cleared.
/// Keeps only the most recent assistant response's thinking blocks.
fn plan_thinking_clearing(events: &[AgentSessionEvent]) -> (HashSet<MessageBlockIndex>, u64) {
    let mut thinking_groups: Vec<(Vec<MessageBlockIndex>, u64)> = Vec::new();
    let mut block_idx = 0usize;

    for event in events {
        match event {
            AgentSessionEvent::UserInputAdded { .. }
            | AgentSessionEvent::SandboxNotificationAdded { .. } => {
                block_idx += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                let mut group_indexes = Vec::new();
                let mut group_tokens = 0u64;

                for block in content {
                    if let AssistantBlock::Thinking { text, .. } = block {
                        group_indexes.push(MessageBlockIndex::new(block_idx));
                        group_tokens += estimation::estimate_event_tokens_for_content(text.len());
                    }
                    block_idx += 1;
                }

                if !group_indexes.is_empty() {
                    thinking_groups.push((group_indexes, group_tokens));
                }
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                block_idx += results.len();
            }
            AgentSessionEvent::CompactionApplied {
                masked_tool_results,
                ..
            } => {
                block_idx += masked_tool_results.len();
            }
            _ => {}
        }
    }

    // Keep the most recent group, clear all earlier ones
    if thinking_groups.len() <= 1 {
        return (HashSet::new(), 0);
    }

    let mut cleared = HashSet::new();
    let mut tokens_saved = 0u64;

    for (indexes, tokens) in &thinking_groups[..thinking_groups.len() - 1] {
        for idx in indexes {
            cleared.insert(*idx);
        }
        tokens_saved += tokens;
    }

    (cleared, tokens_saved)
}

// ============================================================================
// Sandbox notification stripping
// ============================================================================

/// Identify sandbox notifications to strip.
/// Keeps only the most recent Attach for each sandbox that is still attached.
/// Strips all Detach notifications and superseded Attach notifications.
fn plan_sandbox_stripping(events: &[AgentSessionEvent]) -> (HashSet<MessageBlockIndex>, u64) {
    // Track: sandbox_name → (most_recent_attach_block_idx, is_attached)
    let mut sandbox_state: HashMap<String, (MessageBlockIndex, bool)> = HashMap::new();
    // All sandbox notification block indexes seen, in order
    let mut all_sandbox_indexes: Vec<(MessageBlockIndex, String, bool)> = Vec::new();
    let mut block_idx = 0usize;

    for event in events {
        match event {
            AgentSessionEvent::UserInputAdded { .. } => {
                block_idx += 1;
            }
            AgentSessionEvent::SandboxNotificationAdded {
                sandbox_name,
                operation,
                ..
            } => {
                let idx = MessageBlockIndex::new(block_idx);
                let is_attach = matches!(operation, SandboxOperation::Attach { .. });
                all_sandbox_indexes.push((idx, sandbox_name.clone(), is_attach));
                if is_attach {
                    sandbox_state.insert(sandbox_name.clone(), (idx, true));
                } else {
                    sandbox_state
                        .entry(sandbox_name.clone())
                        .and_modify(|s| s.1 = false);
                }
                block_idx += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                block_idx += content.len();
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                block_idx += results.len();
            }
            AgentSessionEvent::CompactionApplied {
                masked_tool_results,
                ..
            } => {
                block_idx += masked_tool_results.len();
            }
            _ => {}
        }
    }

    // Keep: the most recent Attach for each currently-attached sandbox
    let keep: HashSet<MessageBlockIndex> = sandbox_state
        .values()
        .filter(|(_, attached)| *attached)
        .map(|(idx, _)| *idx)
        .collect();

    let mut stripped = HashSet::new();
    let mut tokens_saved = 0u64;

    for (idx, _name, _is_attach) in &all_sandbox_indexes {
        if !keep.contains(idx) {
            stripped.insert(*idx);
            // Sandbox notifications are ~100-200 chars
            tokens_saved += 40;
        }
    }

    (stripped, tokens_saved)
}

// ============================================================================
// SandboxTracker
// ============================================================================

/// Tracks which sandbox was active at each event index in the session.
pub struct SandboxTracker {
    active_at_event: Vec<Option<String>>,
}

impl SandboxTracker {
    pub fn from_events(events: &[AgentSessionEvent]) -> Self {
        let mut current: Option<String> = None;
        let mut active_at_event = Vec::with_capacity(events.len());

        for event in events {
            if let AgentSessionEvent::SandboxNotificationAdded {
                sandbox_name,
                operation,
                ..
            } = event
            {
                match operation {
                    SandboxOperation::Attach { .. } => {
                        current = Some(sandbox_name.clone());
                    }
                    SandboxOperation::Detach => {
                        if current.as_deref() == Some(sandbox_name) {
                            current = None;
                        }
                    }
                }
            }
            active_at_event.push(current.clone());
        }

        Self { active_at_event }
    }

    /// Which sandbox (if any) was active at event index `i`.
    pub fn sandbox_at(&self, i: usize) -> Option<&str> {
        self.active_at_event.get(i)?.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::message::*;
    use crate::agent::session::metadata::*;
    use crate::agent::session::settings::*;

    fn dummy_metadata() -> AssistantResponseMetadata {
        AssistantResponseMetadata {
            api: "test".into(),
            model: "test".into(),
            usage: Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 0,
            },
            cost: Cost::default(),
        }
    }

    fn make_config(keep_recent: usize) -> CompactionConfig {
        CompactionConfig {
            enabled: true,
            token_threshold_fraction: 0.6,
            context_window_tokens: 200_000,
            keep_recent_tool_results: keep_recent,
            cache_ttl_seconds: 300,
        }
    }

    #[test]
    fn empty_plan_when_few_tool_results() {
        use crate::agent::session::thread::SessionThreadId;

        let events = vec![AgentSessionEvent::ToolResultsAdded {
            thread_id: SessionThreadId::new(),
            results: vec![ToolResultInput {
                tool_use_id: "t1".into(),
                content: "short result".into(),
                is_error: false,
            }],
        }];
        let plan = build_pruning_plan(&events, &make_config(10));
        assert!(plan.is_empty());
    }

    #[test]
    fn thinking_clearing_keeps_most_recent() {
        use crate::agent::session::thread::SessionThreadId;

        let thread_id = SessionThreadId::new();
        let events = vec![
            AgentSessionEvent::AssistantResponseReceived {
                thread_id,
                content: vec![
                    AssistantBlock::Thinking {
                        text: "thinking 1".repeat(100),
                        signature: None,
                    },
                    AssistantBlock::Text {
                        text: "response 1".into(),
                    },
                ],
                stop_reason: StopReason::Stop,
                error_message: None,
                metadata: dummy_metadata(),
            },
            AgentSessionEvent::AssistantResponseReceived {
                thread_id,
                content: vec![
                    AssistantBlock::Thinking {
                        text: "thinking 2".repeat(100),
                        signature: None,
                    },
                    AssistantBlock::Text {
                        text: "response 2".into(),
                    },
                ],
                stop_reason: StopReason::Stop,
                error_message: None,
                metadata: dummy_metadata(),
            },
        ];

        let (cleared, _) = plan_thinking_clearing(&events);
        // First thinking (index 0) should be cleared, second (index 2) kept
        assert!(cleared.contains(&MessageBlockIndex::new(0)));
        assert!(!cleared.contains(&MessageBlockIndex::new(2)));
        assert_eq!(cleared.len(), 1);
    }
}
