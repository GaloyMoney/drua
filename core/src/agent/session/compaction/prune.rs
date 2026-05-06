use std::collections::{HashMap, HashSet};

use super::super::entity::{AgentSessionEvent, MaskedToolResult};
use super::super::message::{AssistantBlock, SandboxOperation, ToolResultInput};
use super::super::settings::CompactionConfig;
use super::super::thread::SessionThreadId;
use super::super::view::{MessageBlockIndex, MessageView, PromptDefinition};
use super::{estimation, event_belongs_to_thread};

const MASK_PLACEHOLDER: &str = "[Tool output cleared — re-invoke tool if needed]";

#[derive(Debug, Default)]
pub struct PruningPlan {
    pub masked_tool_results: Vec<MaskedToolResult>,
    pub cleared_thinking: HashSet<MessageBlockIndex>,
    pub stripped_user_messages: HashSet<MessageBlockIndex>,
    pub estimated_tokens_saved: u64,
}

impl PruningPlan {
    pub fn is_empty(&self) -> bool {
        self.masked_tool_results.is_empty()
            && self.cleared_thinking.is_empty()
            && self.stripped_user_messages.is_empty()
    }
}

/// `prompt_definition` resolves which tool results are actually visible —
/// compacted threads inherit results from ancestor threads with old IDs.
pub fn build_pruning_plan<'a>(
    events: impl Iterator<Item = &'a AgentSessionEvent> + Clone,
    thread_id: SessionThreadId,
    is_main_thread: bool,
    config: &CompactionConfig,
    prompt_definition: &PromptDefinition,
) -> PruningPlan {
    let sandbox_tracker = SandboxTracker::from_events(events.clone(), thread_id, is_main_thread);

    let visible_tool_results: HashSet<MessageBlockIndex> = prompt_definition
        .messages
        .iter()
        .filter_map(|m| match m {
            MessageView::ToolResults(v) => Some(v.indexes.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();

    let (masked, mask_tokens) = plan_tool_result_masking(
        events.clone(),
        config.keep_recent_tool_results,
        &sandbox_tracker,
        &visible_tool_results,
    );
    let (cleared, think_tokens) = plan_thinking_clearing(events.clone(), thread_id, is_main_thread);
    let (stripped, sandbox_tokens) = plan_sandbox_stripping(events, thread_id, is_main_thread);

    PruningPlan {
        masked_tool_results: masked,
        cleared_thinking: cleared,
        stripped_user_messages: stripped,
        estimated_tokens_saved: mask_tokens + think_tokens + sandbox_tokens,
    }
}

/// Keeps `keep_recent` most recent intact; masks rest with a placeholder
/// (preserves tool name + sandbox context). Uses `visible_tool_results`
/// rather than event-level ownership to handle inherited results.
fn plan_tool_result_masking<'a>(
    events: impl Iterator<Item = &'a AgentSessionEvent>,
    keep_recent: usize,
    sandbox_tracker: &SandboxTracker,
    visible_tool_results: &HashSet<MessageBlockIndex>,
) -> (Vec<MaskedToolResult>, u64) {
    let mut all_results: Vec<(MessageBlockIndex, usize, &ToolResultInput)> = Vec::new();
    let mut already_masked: HashSet<MessageBlockIndex> = HashSet::new();
    let mut block_idx = 0usize;

    for (event_idx, event) in events.enumerate() {
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
                    let idx = MessageBlockIndex::new(block_idx);
                    if visible_tool_results.contains(&idx) {
                        all_results.push((idx, event_idx, result));
                    }
                    block_idx += 1;
                }
            }
            AgentSessionEvent::ToolResultsMasked { results, .. } => {
                for masked in results {
                    already_masked.insert(masked.original_index);
                }
                block_idx += results.len();
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
        if already_masked.contains(idx) {
            continue;
        }

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

/// Keeps only the most recent assistant response's thinking blocks.
fn plan_thinking_clearing<'a>(
    events: impl Iterator<Item = &'a AgentSessionEvent>,
    thread_id: SessionThreadId,
    is_main_thread: bool,
) -> (HashSet<MessageBlockIndex>, u64) {
    let mut thinking_groups: Vec<(Vec<MessageBlockIndex>, u64)> = Vec::new();
    let mut block_idx = 0usize;

    for event in events {
        match event {
            AgentSessionEvent::UserInputAdded { .. }
            | AgentSessionEvent::SandboxNotificationAdded { .. } => {
                block_idx += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                let belongs = event_belongs_to_thread(event, thread_id, is_main_thread);
                let mut group_indexes = Vec::new();
                let mut group_tokens = 0u64;

                for block in content {
                    if belongs {
                        if let AssistantBlock::Thinking { text, .. } = block {
                            group_indexes.push(MessageBlockIndex::new(block_idx));
                            group_tokens +=
                                estimation::estimate_event_tokens_for_content(text.len());
                        }
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
            AgentSessionEvent::ToolResultsMasked { results, .. } => {
                block_idx += results.len();
            }
            _ => {}
        }
    }

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

/// Keeps only the most recent Attach per still-attached sandbox; strips
/// all Detaches and superseded Attaches.
fn plan_sandbox_stripping<'a>(
    events: impl Iterator<Item = &'a AgentSessionEvent>,
    thread_id: SessionThreadId,
    is_main_thread: bool,
) -> (HashSet<MessageBlockIndex>, u64) {
    // sandbox_name → (most_recent_attach_block_idx, is_attached)
    let mut sandbox_state: HashMap<String, (MessageBlockIndex, bool)> = HashMap::new();
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
                let belongs = event_belongs_to_thread(event, thread_id, is_main_thread);

                if belongs {
                    let is_attach = matches!(operation, SandboxOperation::Attach { .. });
                    all_sandbox_indexes.push((idx, sandbox_name.clone(), is_attach));
                    if is_attach {
                        sandbox_state.insert(sandbox_name.clone(), (idx, true));
                    } else {
                        sandbox_state
                            .entry(sandbox_name.clone())
                            .and_modify(|s| s.1 = false);
                    }
                }
                block_idx += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                block_idx += content.len();
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                block_idx += results.len();
            }
            AgentSessionEvent::ToolResultsMasked { results, .. } => {
                block_idx += results.len();
            }
            _ => {}
        }
    }

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
            tokens_saved += 40; // sandbox notifications are ~100-200 chars
        }
    }

    (stripped, tokens_saved)
}

/// Active sandbox at each event index (filtered to thread's events).
pub struct SandboxTracker {
    active_at_event: Vec<Option<String>>,
}

impl SandboxTracker {
    pub fn from_events<'a>(
        events: impl Iterator<Item = &'a AgentSessionEvent>,
        thread_id: SessionThreadId,
        is_main_thread: bool,
    ) -> Self {
        let mut current: Option<String> = None;
        let mut active_at_event = Vec::new();

        for event in events {
            if let AgentSessionEvent::SandboxNotificationAdded {
                sandbox_name,
                operation,
                ..
            } = event
            {
                if event_belongs_to_thread(event, thread_id, is_main_thread) {
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
            }
            active_at_event.push(current.clone());
        }

        Self { active_at_event }
    }

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
    use crate::agent::session::view::{SystemView, ToolDefinitionsView, ToolResultsView};

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
                reasoning: 0,
            },
            cost: Cost::default(),
        }
    }

    fn make_config(keep_recent: usize) -> CompactionConfig {
        CompactionConfig {
            enabled: true,
            token_threshold_fraction: 0.6,
            keep_recent_tool_results: keep_recent,
            reset_time_delta_seconds: None,
            prune_after_seconds: Some(300),
        }
    }

    fn prompt_with_tool_results(indexes: &[usize]) -> PromptDefinition {
        let messages = if indexes.is_empty() {
            vec![]
        } else {
            vec![MessageView::ToolResults(ToolResultsView {
                indexes: indexes.iter().map(|&i| MessageBlockIndex::new(i)).collect(),
            })]
        };
        PromptDefinition {
            model: String::new(),
            max_tokens_per_response: 0,
            system_view: SystemView { indexes: vec![] },
            tool_definitions_view: ToolDefinitionsView { indexes: vec![] },
            messages,
            compaction: None,
        }
    }

    #[test]
    fn empty_plan_when_few_tool_results() {
        let thread_id = SessionThreadId::new();

        let events = [AgentSessionEvent::ToolResultsAdded {
            thread_id,
            results: vec![ToolResultInput {
                tool_use_id: "t1".into(),
                content: "short result".into(),
                is_error: false,
            }],
        }];
        let prompt = prompt_with_tool_results(&[0]);
        let plan = build_pruning_plan(events.iter(), thread_id, true, &make_config(10), &prompt);
        assert!(plan.is_empty());
    }

    #[test]
    fn thinking_clearing_keeps_most_recent() {
        let thread_id = SessionThreadId::new();

        let events = [
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

        let (cleared, _) = plan_thinking_clearing(events.iter(), thread_id, true);
        assert!(cleared.contains(&MessageBlockIndex::new(0)));
        assert!(!cleared.contains(&MessageBlockIndex::new(2)));
        assert_eq!(cleared.len(), 1);
    }

    fn large_content() -> String {
        "x".repeat(500)
    }

    #[test]
    fn masking_skips_already_masked_tool_results() {
        let thread_id = SessionThreadId::new();

        // 3 results, keep_recent=1; prior mask covers idx 0 → only idx 1 should mask.
        let events = [
            AgentSessionEvent::ToolResultsAdded {
                thread_id,
                results: vec![ToolResultInput {
                    tool_use_id: "t1".into(),
                    content: large_content(),
                    is_error: false,
                }],
            },
            AgentSessionEvent::ToolResultsAdded {
                thread_id,
                results: vec![ToolResultInput {
                    tool_use_id: "t2".into(),
                    content: large_content(),
                    is_error: false,
                }],
            },
            AgentSessionEvent::ToolResultsMasked {
                results: vec![MaskedToolResult {
                    original_index: MessageBlockIndex::new(0),
                    replacement: ToolResultInput {
                        tool_use_id: "t1".into(),
                        content: "[Tool output cleared — re-invoke tool if needed]".into(),
                        is_error: false,
                    },
                }],
            },
            AgentSessionEvent::ToolResultsAdded {
                thread_id,
                results: vec![ToolResultInput {
                    tool_use_id: "t3".into(),
                    content: large_content(),
                    is_error: false,
                }],
            },
        ];

        let prompt = prompt_with_tool_results(&[2, 1, 3]);
        let plan = build_pruning_plan(events.iter(), thread_id, true, &make_config(1), &prompt);

        assert_eq!(plan.masked_tool_results.len(), 1);
        assert_eq!(
            plan.masked_tool_results[0].original_index,
            MessageBlockIndex::new(1)
        );
    }
}
