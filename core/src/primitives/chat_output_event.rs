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
    /// Incremental text token from a streaming assistant response.
    AssistantTextDelta {
        text: String,
    },
    /// Incremental thinking token from a streaming assistant response.
    ThinkingDelta {
        text: String,
    },
    /// Signals the start of a tool call in a streaming response.
    ToolCallStart {
        name: String,
    },
    /// Incremental tool-call input JSON fragment.
    ToolCallInputDelta {
        partial_json: String,
    },
    ToolResult {
        name: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
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
    /// Workflow agent step submitted its structured output via the
    /// synthesised `submit_output` tool. Workflow executor consumes
    /// this to populate `StepResult.output`.
    OutputSubmitted {
        value: serde_json::Value,
    },
}
