use serde::{Deserialize, Serialize};

use super::thread::SessionThreadId;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetThread {
    Main,
    Id(SessionThreadId),
}

/// Metadata attached to a prompt built from a compacted thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionMetadata {
    pub follows_from: SessionThreadId,
    pub tool_results_masked: usize,
    pub thinking_blocks_cleared: usize,
    pub sandbox_notifications_stripped: usize,
    pub estimated_tokens_saved: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub target_thread: TargetThread,
    pub model: String,
    pub max_tokens_per_response: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User { content: Vec<UserBlock> },
    Assistant { content: Vec<AssistantBlock> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserBlock {
    Text {
        text: String,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultBlock>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    SandboxInfo {
        sandbox_name: String,
        sandbox_operation: SandboxOperation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Thinking {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultInput {
    pub tool_use_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
}

// ============================================================================
// Conversions: message types → llm types
// ============================================================================

impl From<Prompt> for llm::Prompt {
    fn from(p: Prompt) -> Self {
        llm::Prompt {
            model: p.model,
            max_tokens: Some(p.max_tokens_per_response),
            cache_key: p.cache_key,
            system: p
                .system
                .into_iter()
                .map(llm::prompt::SystemBlock::from)
                .collect(),
            tools: p.tools.into_iter().map(llm::prompt::Tool::from).collect(),
            tool_choice: None,
            messages: p
                .messages
                .into_iter()
                .map(llm::prompt::Message::from)
                .collect(),
        }
    }
}

impl From<SystemBlock> for llm::prompt::SystemBlock {
    fn from(b: SystemBlock) -> Self {
        match b {
            SystemBlock::Text { text } => llm::prompt::SystemBlock::Text {
                text,
                cache_control: None,
            },
        }
    }
}

impl From<ToolDefinition> for llm::prompt::Tool {
    fn from(t: ToolDefinition) -> Self {
        llm::prompt::Tool {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
            cache_control: None,
        }
    }
}

impl From<Message> for llm::prompt::Message {
    fn from(m: Message) -> Self {
        match m {
            Message::User { content } => llm::prompt::Message::User {
                content: content
                    .into_iter()
                    .map(llm::prompt::UserBlock::from)
                    .collect(),
            },
            Message::Assistant { content } => llm::prompt::Message::Assistant {
                content: content
                    .into_iter()
                    .map(llm::prompt::AssistantBlock::from)
                    .collect(),
            },
        }
    }
}

impl From<UserBlock> for llm::prompt::UserBlock {
    fn from(b: UserBlock) -> Self {
        match b {
            UserBlock::Text { text } => llm::prompt::UserBlock::Text {
                text,
                cache_control: None,
            },
            UserBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => llm::prompt::UserBlock::ToolResult {
                tool_use_id,
                content: content
                    .into_iter()
                    .map(|tb| match tb {
                        ToolResultBlock::Text { text } => {
                            llm::prompt::ToolResultBlock::Text { text }
                        }
                    })
                    .collect(),
                is_error,
                cache_control: None,
            },
            UserBlock::SandboxInfo {
                sandbox_name,
                sandbox_operation,
            } => llm::prompt::UserBlock::Text {
                text: sandbox_notification_text(&sandbox_name, &sandbox_operation),
                cache_control: None,
            },
        }
    }
}

impl From<AssistantBlock> for llm::prompt::AssistantBlock {
    fn from(b: AssistantBlock) -> Self {
        match b {
            AssistantBlock::Text { text } => llm::prompt::AssistantBlock::Text {
                text,
                cache_control: None,
            },
            AssistantBlock::ToolUse { id, name, input } => llm::prompt::AssistantBlock::ToolUse {
                id,
                name,
                input,
                cache_control: None,
            },
            AssistantBlock::Thinking { text, signature } => {
                llm::prompt::AssistantBlock::Thinking { text, signature }
            }
        }
    }
}

// ============================================================================
// Conversions: llm types → message types
// ============================================================================

impl From<llm::prompt::AssistantBlock> for AssistantBlock {
    fn from(b: llm::prompt::AssistantBlock) -> Self {
        match b {
            llm::prompt::AssistantBlock::Text { text, .. } => AssistantBlock::Text { text },
            llm::prompt::AssistantBlock::ToolUse {
                id, name, input, ..
            } => AssistantBlock::ToolUse { id, name, input },
            llm::prompt::AssistantBlock::Thinking { text, signature } => {
                AssistantBlock::Thinking { text, signature }
            }
        }
    }
}

impl From<llm::response::StopReason> for StopReason {
    fn from(s: llm::response::StopReason) -> Self {
        match s {
            llm::response::StopReason::EndTurn => StopReason::Stop,
            llm::response::StopReason::MaxTokens => StopReason::Length,
            llm::response::StopReason::StopSequence => StopReason::Stop,
            llm::response::StopReason::ToolUse => StopReason::ToolUse,
        }
    }
}

impl From<llm::ToolUseResult> for ToolResultInput {
    fn from(r: llm::ToolUseResult) -> Self {
        Self {
            tool_use_id: r.tool_use_id,
            content: r.content,
            is_error: r.is_error,
        }
    }
}

impl From<llm::prompt::Tool> for ToolDefinition {
    fn from(t: llm::prompt::Tool) -> Self {
        Self {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        }
    }
}

impl From<llm::prompt::SystemBlock> for SystemBlock {
    fn from(b: llm::prompt::SystemBlock) -> Self {
        match b {
            llm::prompt::SystemBlock::Text { text, .. } => SystemBlock::Text { text },
        }
    }
}

// ============================================================================
// Sandbox notification helpers
// ============================================================================

/// What happened to a sandbox from the agent's perspective.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxOperation {
    Attach { mode: String, mount_path: String },
    Detach,
}

/// Build the XML text injected into the agent's session when a sandbox is
/// attached or detached. Matches the `<sandbox>` tag format described in the
/// agent system prompt.
pub fn sandbox_notification_text(sandbox_name: &str, op: &SandboxOperation) -> String {
    match op {
        SandboxOperation::Attach { mode, mount_path } => {
            format!(
                "<sandbox>\n\
                 Attached sandbox \"{sandbox_name}\" in {mode} mode.\n\
                 The workspace is mounted at {mount_path}. \
                 All file operations and command execution are confined to this path — \
                 do not attempt to read, write, or execute anything outside it.\n\
                 </sandbox>"
            )
        }
        SandboxOperation::Detach => {
            format!("<sandbox>\nDetached sandbox \"{sandbox_name}\".\n</sandbox>")
        }
    }
}
