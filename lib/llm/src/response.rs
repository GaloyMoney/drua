use serde::{Deserialize, Serialize};

use crate::prompt::AssistantBlock;

/// A single response from an LLM. Provider clients (`anthropic-client`,
/// `openai-client`, …) build this from their wire format; downstream code
/// (executor, agent) only ever sees this provider-agnostic shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
    pub content: Vec<AssistantBlock>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Total input tokens seen by the model. Providers that expose cache hits
    /// or cache writes separately should still populate this with the full
    /// prompt token count so downstream accounting stays consistent.
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
}

/// A tool call extracted from an assistant response that the caller is
/// expected to dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}
