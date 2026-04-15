use serde::Serialize;

/// Event streamed back to callers while an agent processes a message.
/// Carried over an mpsc channel and serialized as SSE on the web layer.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatOutputEvent {
    UserMessage {
        source: super::UserMessageSource,
        text: String,
    },
    AssistantText {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
    },
    ToolResult {
        name: String,
        is_error: bool,
    },
    AssistantDone {
        turns: u32,
        input_tokens: u32,
        output_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    Error {
        message: String,
    },
    /// Infrastructure status update (e.g. sandbox provisioning, executor
    /// reconnection).
    Service {
        message: String,
    },
}
