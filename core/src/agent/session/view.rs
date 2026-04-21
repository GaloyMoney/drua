use serde::{Deserialize, Serialize};

use es_entity::EntityEvents;

use crate::agent::config::ModelDefaults;

use super::{entity::AgentSessionEvent, error::AgentSessionError, message::*};

// ============================================================================
// Index types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SystemBlockIndex(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolDefinitionIndex(usize);

/// A unified index that increments across all message block types
/// (user messages, assistant blocks, and tool results).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MessageBlockIndex(usize);

impl MessageBlockIndex {
    pub(super) fn new(idx: usize) -> Self {
        Self(idx)
    }

    pub(super) fn index(&self) -> usize {
        self.0
    }
}

// ============================================================================
// View types (persisted on threads)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemView {
    pub(super) indexes: Vec<SystemBlockIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinitionsView {
    pub(super) indexes: Vec<ToolDefinitionIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessagesView {
    pub(super) indexes: Vec<MessageBlockIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageView {
    pub(super) indexes: Vec<MessageBlockIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultsView {
    pub(super) indexes: Vec<MessageBlockIndex>,
}

// ============================================================================
// MaterializedSession
// ============================================================================

#[derive(Debug)]
pub(super) struct MaterializedSession<'a> {
    model_defaults: &'a ModelDefaults,
    system_blocks: Vec<&'a SystemBlock>,
    system_breakpoints: Vec<SystemBlockIndex>,
    tool_defs: Vec<&'a ToolDefinition>,
    tool_breakpoints: Vec<ToolDefinitionIndex>,
    block_count: usize,
    user_message_indexes: Vec<MessageBlockIndex>,
    assistant_breakpoints: Vec<MessageBlockIndex>,
    tool_result_breakpoints: Vec<MessageBlockIndex>,
}

impl<'a> MaterializedSession<'a> {
    pub fn init(model_defaults: &'a ModelDefaults) -> Self {
        Self {
            model_defaults,
            system_blocks: Vec::new(),
            system_breakpoints: Vec::new(),
            tool_defs: Vec::new(),
            tool_breakpoints: Vec::new(),
            block_count: 0,
            user_message_indexes: Vec::new(),
            assistant_breakpoints: Vec::new(),
            tool_result_breakpoints: Vec::new(),
        }
    }

    pub fn push_system_blocks(&mut self, blocks: impl Iterator<Item = &'a SystemBlock>) {
        self.system_breakpoints
            .push(SystemBlockIndex(self.system_blocks.len()));
        self.system_blocks.extend(blocks);
    }

    pub fn push_tool_defs(&mut self, defs: impl Iterator<Item = &'a ToolDefinition>) {
        self.tool_breakpoints
            .push(ToolDefinitionIndex(self.tool_defs.len()));
        self.tool_defs.extend(defs);
    }

    pub fn push_user_message(&mut self) {
        let idx = MessageBlockIndex(self.block_count);
        self.block_count += 1;
        self.user_message_indexes.push(idx);
    }

    pub fn push_assistant_blocks(&mut self, count: usize) {
        self.assistant_breakpoints
            .push(MessageBlockIndex(self.block_count));
        self.block_count += count;
    }

    /// Returns the assistant block indexes from the most recent
    /// `push_assistant_blocks` call.
    ///
    /// **Invariant**: must be called before any subsequent `push_*` call
    /// that would advance `block_count`.
    pub fn assistant_blocks_since_last_breakpoint(&self) -> AssistantMessageView {
        let start = self
            .assistant_breakpoints
            .last()
            .map(|idx| idx.0)
            .unwrap_or(0);
        AssistantMessageView {
            indexes: (start..self.block_count).map(MessageBlockIndex).collect(),
        }
    }

    pub fn push_tool_results(&mut self, count: usize) {
        self.tool_result_breakpoints
            .push(MessageBlockIndex(self.block_count));
        self.block_count += count;
    }

    /// Returns the tool result indexes from the most recent
    /// `push_tool_results` call.
    ///
    /// **Invariant**: must be called before any subsequent `push_*` call
    /// that would advance `block_count`.
    pub fn tool_results_since_last_breakpoint(&self) -> ToolResultsView {
        let start = self
            .tool_result_breakpoints
            .last()
            .map(|idx| idx.0)
            .unwrap_or(0);
        ToolResultsView {
            indexes: (start..self.block_count).map(MessageBlockIndex).collect(),
        }
    }

    fn system_since_last_breakpoint(&self) -> SystemView {
        let start = self.system_breakpoints.last().map(|idx| idx.0).unwrap_or(0);
        SystemView {
            indexes: (start..self.system_blocks.len())
                .map(SystemBlockIndex)
                .collect(),
        }
    }

    pub fn tools_since_last_breakpoint(&self) -> ToolDefinitionsView {
        let start = self.tool_breakpoints.last().map(|idx| idx.0).unwrap_or(0);
        ToolDefinitionsView {
            indexes: (start..self.tool_defs.len())
                .map(ToolDefinitionIndex)
                .collect(),
        }
    }

    pub fn all_user_messages(&self) -> UserMessagesView {
        UserMessagesView {
            indexes: self.user_message_indexes.clone(),
        }
    }

    pub fn initial_prompt_definition(self) -> PromptDefinition {
        let system_view = self.system_since_last_breakpoint();
        let tool_definitions_view = self.tools_since_last_breakpoint();
        let initial_user_messages = self.all_user_messages();
        PromptDefinition {
            model: self.model_defaults.model.clone(),
            max_tokens_per_response: self.model_defaults.max_tokens_per_response,
            system_view,
            tool_definitions_view,
            messages: vec![MessageView::User(initial_user_messages)],
            compaction: None,
        }
    }
}

// ============================================================================
// BlockContent (used internally for prompt resolution)
// ============================================================================

enum BlockContent {
    UserText(String),
    AssistantBlock(AssistantBlock),
    ToolResult(ToolResultInput),
}

// ============================================================================
// PromptDefinition
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageView {
    User(UserMessagesView),
    Assistant(AssistantMessageView),
    ToolResults(ToolResultsView),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDefinition {
    pub(super) model: String,
    pub(super) max_tokens_per_response: u32,
    pub(super) system_view: SystemView,
    pub(super) tool_definitions_view: ToolDefinitionsView,
    pub(super) messages: Vec<MessageView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) compaction: Option<CompactionMetadata>,
}

impl PromptDefinition {
    pub fn system_view(&self) -> &SystemView {
        &self.system_view
    }

    pub fn tool_definitions_view(&self) -> &ToolDefinitionsView {
        &self.tool_definitions_view
    }

    pub fn user_messages_view(&self) -> UserMessagesView {
        let mut indexes = Vec::new();
        for msg in &self.messages {
            match msg {
                MessageView::User(view) => indexes.extend_from_slice(&view.indexes),
                MessageView::Assistant(_) | MessageView::ToolResults(_) => {}
            }
        }
        UserMessagesView { indexes }
    }

    pub fn into_prompt(
        self,
        target_thread: TargetThread,
        events: &EntityEvents<AgentSessionEvent>,
    ) -> Result<Prompt, AgentSessionError> {
        let mut all_system_blocks = Vec::new();
        let mut all_tool_defs = Vec::new();
        let mut all_blocks: Vec<BlockContent> = Vec::new();

        for event in events.iter_all() {
            match event {
                AgentSessionEvent::Initialized {
                    system_blocks,
                    tool_defs,
                    ..
                } => {
                    all_system_blocks.extend(system_blocks.iter().cloned());
                    all_tool_defs.extend(tool_defs.iter().cloned());
                }
                AgentSessionEvent::SystemBlocksUpdated { system_blocks } => {
                    all_system_blocks.extend(system_blocks.iter().cloned());
                }
                AgentSessionEvent::ToolDefsUpdated { tool_defs } => {
                    all_tool_defs.extend(tool_defs.iter().cloned());
                }
                AgentSessionEvent::UserInputAdded { text, .. } => {
                    all_blocks.push(BlockContent::UserText(text.clone()));
                }
                AgentSessionEvent::SandboxNotificationAdded { text, .. } => {
                    all_blocks.push(BlockContent::UserText(text.clone()));
                }
                AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                    for block in content {
                        // Sanitize null tool_use inputs (from events persisted before
                        // the stream accumulator fix) — the Anthropic API rejects
                        // `"input": null`, requiring a valid JSON object.
                        let block = match block {
                            AssistantBlock::ToolUse { id, name, input }
                                if input.is_null() =>
                            {
                                AssistantBlock::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: serde_json::Value::Object(serde_json::Map::new()),
                                }
                            }
                            other => other.clone(),
                        };
                        all_blocks.push(BlockContent::AssistantBlock(block));
                    }
                }
                AgentSessionEvent::ToolResultsAdded { results, .. } => {
                    for result in results {
                        all_blocks.push(BlockContent::ToolResult(result.clone()));
                    }
                }
                AgentSessionEvent::ToolResultsMasked { results, .. } => {
                    for masked in results {
                        all_blocks.push(BlockContent::ToolResult(masked.replacement.clone()));
                    }
                }
                _ => {}
            }
        }

        let system = self
            .system_view
            .indexes
            .iter()
            .map(|idx| all_system_blocks[idx.0].clone())
            .collect();

        let tools = self
            .tool_definitions_view
            .indexes
            .iter()
            .map(|idx| all_tool_defs[idx.0].clone())
            .collect();

        let mut messages = Vec::new();
        for msg_view in &self.messages {
            match msg_view {
                MessageView::User(user_view) => {
                    let content = user_view
                        .indexes
                        .iter()
                        .map(|idx| match &all_blocks[idx.0] {
                            BlockContent::UserText(text) => UserBlock::Text { text: text.clone() },
                            _ => {
                                panic!("BlockIndex in UserMessagesView does not point to UserText")
                            }
                        })
                        .collect();
                    messages.push(Message::User { content });
                }
                MessageView::Assistant(assistant_view) => {
                    let content = assistant_view
                        .indexes
                        .iter()
                        .map(|idx| match &all_blocks[idx.0] {
                            BlockContent::AssistantBlock(block) => block.clone(),
                            _ => panic!(
                                "BlockIndex in AssistantMessageView does not point to AssistantBlock"
                            ),
                        })
                        .collect();
                    messages.push(Message::Assistant { content });
                }
                MessageView::ToolResults(tool_results_view) => {
                    let content = tool_results_view
                        .indexes
                        .iter()
                        .map(|idx| match &all_blocks[idx.0] {
                            BlockContent::ToolResult(result) => UserBlock::ToolResult {
                                tool_use_id: result.tool_use_id.clone(),
                                content: vec![ToolResultBlock::Text {
                                    text: result.content.clone(),
                                }],
                                is_error: result.is_error,
                            },
                            _ => {
                                panic!("BlockIndex in ToolResultsView does not point to ToolResult")
                            }
                        })
                        .collect();
                    messages.push(Message::User { content });
                }
            }
        }

        // Merge consecutive User messages (e.g. tool results followed by user text)
        let mut merged: Vec<Message> = Vec::new();
        for msg in messages {
            match msg {
                Message::User { content } => {
                    if let Some(Message::User {
                        content: existing, ..
                    }) = merged.last_mut()
                    {
                        existing.extend(content);
                    } else {
                        merged.push(Message::User { content });
                    }
                }
                other => merged.push(other),
            }
        }

        Ok(Prompt {
            target_thread,
            model: self.model,
            max_tokens_per_response: self.max_tokens_per_response,
            system,
            tools,
            messages: merged,
            compaction: self.compaction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: None,
            input_schema: serde_json::json!({}),
        }
    }

    fn system_block(text: &str) -> SystemBlock {
        SystemBlock::Text {
            text: text.to_string(),
        }
    }

    fn test_model_defaults() -> ModelDefaults {
        ModelDefaults {
            model: "test-model".into(),
            max_tokens_per_response: 8192,
            context_window_tokens: 200_000,
        }
    }

    #[test]
    fn tool_defs_breakpoint_returns_latest_batch() {
        let tool_a = tool_def("tool_a");
        let tool_b = tool_def("tool_b");
        let tool_c = tool_def("tool_c");
        let tool_d = tool_def("tool_d");

        let defaults = test_model_defaults();
        let mut m = MaterializedSession::init(&defaults);
        m.push_tool_defs([&tool_a, &tool_b].into_iter());
        m.push_tool_defs([&tool_c, &tool_d].into_iter());

        let view = m.tools_since_last_breakpoint();
        assert_eq!(view.indexes.len(), 2);
    }

    #[test]
    fn system_blocks_breakpoint_returns_latest_batch() {
        let block_a = system_block("You are helpful.");
        let block_b = system_block("Be concise.");
        let block_c = system_block("Use examples.");

        let defaults = test_model_defaults();
        let mut m = MaterializedSession::init(&defaults);
        m.push_system_blocks([&block_a].into_iter());
        m.push_system_blocks([&block_b, &block_c].into_iter());

        let view = m.system_since_last_breakpoint();
        assert_eq!(view.indexes.len(), 2);
    }
}
