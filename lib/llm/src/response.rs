use serde::{Deserialize, Serialize};

use crate::prompt::AssistantBlock;

/// Provider-agnostic LLM response.
///
/// `model_used` carries the actual model id that produced this response —
/// distinct from the *primary* of the chain when fallback fired. The chain
/// walker (`crate::router`) populates it; bare client calls leave it `None`
/// (caller may assume the chain primary).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptResponse {
    pub content: Vec<AssistantBlock>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
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
