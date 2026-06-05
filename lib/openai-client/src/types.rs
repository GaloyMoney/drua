//! OpenAI Chat Completions wire types. The public boundary uses the
//! provider-agnostic types from `lib/llm`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAiToolChoice>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReasoningConfig {
    pub effort: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenAiMessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiRequestToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// String form is the Chat Completions default; the array form is required
/// when any block needs `cache_control` (Anthropic-on-OpenRouter).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum OpenAiMessageContent {
    Text(String),
    Blocks(Vec<OpenAiContentBlock>),
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiContentBlock {
    pub r#type: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<OpenAiCacheControl>,
}

/// OpenRouter forwards this to Anthropic verbatim; direct OpenAI ignores it.
/// `ttl` defaults to 5 minutes when omitted; `"1h"` is the only other value.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OpenAiCacheControl {
    pub r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiRequestToolCall {
    pub id: String,
    pub r#type: &'static str,
    pub function: OpenAiFunction,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiTool {
    pub r#type: &'static str,
    pub function: OpenAiToolFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<OpenAiCacheControl>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum OpenAiToolChoice {
    String(&'static str),
    Specific {
        r#type: &'static str,
        function: OpenAiToolChoiceFunction,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamChunk {
    #[serde(default)]
    pub choices: Vec<OpenAiChoice>,
    #[serde(default)]
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiChoice {
    #[serde(default)]
    pub delta: Option<OpenAiDelta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<OpenAiFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<OpenAiCompletionTokensDetails>,
    /// OpenRouter populates this with USD credits charged. Direct OpenAI
    /// never sets it; presence is the OpenRouter signal.
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub cost_details: Option<OpenAiCostDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
    /// OpenRouter populates this for upstream models with explicit cache-write
    /// pricing (Anthropic family). OpenAI direct never sets it.
    #[serde(default)]
    pub cache_write_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiCompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiCostDetails {
    /// Actual upstream provider cost; OpenRouter BYOK only.
    #[serde(default)]
    pub upstream_inference_cost: Option<f64>,
}
