use serde::{Deserialize, Serialize};

use super::new_thread::SessionThreadId;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetThread {
    Main,
    Id(SessionThreadId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub target_thread: TargetThread,
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
}
