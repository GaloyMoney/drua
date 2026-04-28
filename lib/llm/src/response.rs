use serde::{Deserialize, Serialize};

use crate::prompt::AssistantBlock;

/// Provider-agnostic LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
    pub content: Vec<AssistantBlock>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Always the full prompt token count (cache hits/writes included), so
    /// downstream accounting stays consistent across providers.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}
