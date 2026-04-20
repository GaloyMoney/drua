//! OpenAI Chat Completions API wire types.
//!
//! These types model the OpenAI wire format for both requests and streaming
//! responses. They are used internally by `OpenAiClient` and are NOT exposed
//! to callers — the public boundary uses the provider-agnostic types from
//! `lib/llm`.

use serde::{Deserialize, Serialize};

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
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
}

#[derive(Debug, Serialize)]
pub(crate) struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiRequestToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
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

// ============================================================================
// Streaming Response Types
// ============================================================================

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
}
