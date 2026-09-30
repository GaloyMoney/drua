use serde::Serialize;

/// Why a run-owned agent's dispatch loop stopped short of `AssistantDone`'s
/// usual `Done`/turn-budget outcomes. Carried on `AssistantDone` so the
/// workflow executor can tell a budget stop apart from "no submit_output
/// yet, try a continuation" (handoff-workflow-max-cost-usd-2026-09-30.md §3-4).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStopKind {
    /// The remaining-budget ceiling passed into this dispatch was
    /// already exhausted before (or reached after) a turn.
    LimitReached,
    /// A turn completed or failed without the provider reporting a
    /// cost, so this invocation's true spend can't be verified.
    CostMeteringUnavailable,
}

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
        /// Total cost across every turn in this invocation. `None` means
        /// at least one turn's cost is unknown (e.g. a direct-Anthropic
        /// turn, which never reports one) — distinct from a fully-known
        /// total of exactly `0.0`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// Set when a run-owned agent's dispatch loop stopped early
        /// because of its passed-in budget ceiling, rather than reaching
        /// a natural `Done`/no-submit_output outcome.
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_stopped: Option<BudgetStopKind>,
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
