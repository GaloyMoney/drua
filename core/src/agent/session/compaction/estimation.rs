use super::super::entity::AgentSessionEvent;
use super::super::message::sandbox_notification_text;
use super::super::metadata::Usage;

const CHARS_PER_TOKEN: usize = 4;

/// Estimate context tokens from session events.
///
/// Strategy: use the most recent `Usage` from an `AssistantResponseReceived`
/// event as baseline (its `input` field = how many tokens the LLM saw),
/// then add char-based estimates for events that arrived since.
pub fn estimate_context_tokens(events: &[AgentSessionEvent], last_usage: Option<&Usage>) -> u64 {
    if let Some(usage) = last_usage {
        let baseline = usage.input + usage.output;
        let trailing = estimate_trailing_tokens(events, usage);
        baseline.saturating_add(trailing)
    } else {
        estimate_all_tokens(events)
    }
}

/// Estimate tokens for events after the last known usage checkpoint.
///
/// We walk backwards to find the last `AssistantResponseReceived` that
/// matches the usage, then estimate everything after it.
fn estimate_trailing_tokens(events: &[AgentSessionEvent], _usage: &Usage) -> u64 {
    // Find the index of the last AssistantResponseReceived
    let last_response_idx = events
        .iter()
        .rposition(|e| matches!(e, AgentSessionEvent::AssistantResponseReceived { .. }));

    let start = match last_response_idx {
        Some(idx) => idx + 1,
        None => return estimate_all_tokens(events),
    };

    events[start..].iter().map(estimate_event_tokens).sum()
}

/// Estimate tokens for all events (fallback when no usage baseline exists).
fn estimate_all_tokens(events: &[AgentSessionEvent]) -> u64 {
    events.iter().map(estimate_event_tokens).sum()
}

fn estimate_event_tokens(event: &AgentSessionEvent) -> u64 {
    match event {
        AgentSessionEvent::UserInputAdded { text, .. } => chars_to_tokens(text.len()),
        AgentSessionEvent::SandboxNotificationAdded {
            sandbox_name,
            operation,
            ..
        } => {
            let text = sandbox_notification_text(sandbox_name, operation);
            chars_to_tokens(text.len())
        }
        AgentSessionEvent::AssistantResponseReceived { content, .. } => {
            content.iter().map(estimate_assistant_block_tokens).sum()
        }
        AgentSessionEvent::ToolResultsAdded { results, .. } => results
            .iter()
            .map(|r| chars_to_tokens(r.content.len()))
            .sum(),
        AgentSessionEvent::CompactionApplied {
            masked_tool_results,
            ..
        } => masked_tool_results
            .iter()
            .map(|m| chars_to_tokens(m.replacement.content.len()))
            .sum(),
        _ => 0,
    }
}

fn estimate_assistant_block_tokens(block: &super::super::message::AssistantBlock) -> u64 {
    match block {
        super::super::message::AssistantBlock::Text { text } => chars_to_tokens(text.len()),
        super::super::message::AssistantBlock::ToolUse { name, input, .. } => {
            let json = serde_json::to_string(input).unwrap_or_default();
            chars_to_tokens(name.len() + json.len())
        }
        super::super::message::AssistantBlock::Thinking { text, .. } => chars_to_tokens(text.len()),
    }
}

fn chars_to_tokens(chars: usize) -> u64 {
    (chars / CHARS_PER_TOKEN).max(1) as u64
}

/// Estimate tokens for a given content length (in chars).
/// Exposed for use by the pruning module.
pub fn estimate_event_tokens_for_content(content_len: usize) -> u64 {
    chars_to_tokens(content_len)
}

/// Extract the most recent `Usage` from the session event stream.
pub fn last_usage(events: &[AgentSessionEvent]) -> Option<&Usage> {
    events.iter().rev().find_map(|e| match e {
        AgentSessionEvent::AssistantResponseReceived { metadata, .. } => Some(&metadata.usage),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars_to_tokens_minimum_one() {
        assert_eq!(chars_to_tokens(0), 1);
        assert_eq!(chars_to_tokens(1), 1);
        assert_eq!(chars_to_tokens(4), 1);
        assert_eq!(chars_to_tokens(8), 2);
    }

    #[test]
    fn estimate_empty_events() {
        assert_eq!(estimate_context_tokens(&[], None), 0);
    }
}
