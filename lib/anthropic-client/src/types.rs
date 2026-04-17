//! Anthropic Messages API types ported from the Pi agent crate.
//!
//! These types model the Anthropic wire format for both requests and streaming
//! responses. They are used internally by `AnthropicClient` and are NOT exposed
//! to callers — the public boundary uses the provider-agnostic types from
//! `lib/llm`.

use serde::{Deserialize, Serialize};

// ============================================================================
// Request Types
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicSystemBlock>>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicThinking {
    pub r#type: &'static str,
    pub budget_tokens: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicMessage {
    pub role: &'static str,
    pub content: Vec<AnthropicContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    #[allow(dead_code)]
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<AnthropicToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AnthropicCacheControl {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicImageSource {
    pub r#type: &'static str,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicToolResultContent {
    Text {
        text: String,
    },
    #[allow(dead_code)]
    Image {
        source: AnthropicImageSource,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct AnthropicSystemBlock {
    pub r#type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

// ============================================================================
// Streaming Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageStart,
    },
    ContentBlockStart {
        #[allow(dead_code)]
        index: u32,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicDeltaUsage>,
    },
    MessageStop,
    Error {
        error: AnthropicErrorBody,
    },
    Ping,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicMessageStart {
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

/// Usage statistics from Anthropic API.
/// Field names match the API response format.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
pub(crate) struct AnthropicUsage {
    #[serde(rename = "input_tokens")]
    pub input: u64,
    #[serde(default, rename = "cache_read_input_tokens")]
    pub cache_read: Option<u64>,
    #[serde(default, rename = "cache_creation_input_tokens")]
    pub cache_write: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicDeltaUsage {
    pub output_tokens: u64,
}

/// Content block type from `content_block_start`.
///
/// Using a tagged enum avoids allocating a `String` for the type field.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicContentBlock {
    Text,
    Thinking,
    ToolUse {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
}

/// Per-token delta from the Anthropic streaming API.
///
/// Using a tagged enum instead of a flat struct with `r#type: String` avoids
/// allocating a `String` for the type discriminant on every content_block_delta
/// event (the hottest path -- one allocation per streamed token).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum AnthropicDelta {
    TextDelta {
        #[serde(default)]
        text: Option<String>,
    },
    ThinkingDelta {
        #[serde(default)]
        thinking: Option<String>,
    },
    InputJsonDelta {
        #[serde(default)]
        partial_json: Option<String>,
    },
    SignatureDelta {
        #[serde(default)]
        signature: Option<String>,
    },
}

/// Stop reason from `message_delta`.
///
/// Using an enum avoids allocating a `String` for the stop reason.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnthropicStopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<AnthropicStopReason>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnthropicErrorBody {
    pub message: String,
}
