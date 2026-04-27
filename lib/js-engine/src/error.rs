use thiserror::Error;

#[derive(Error, Debug)]
pub enum JsEngineError {
    #[error("JsEngineError - Runtime: {0}")]
    Runtime(String),
    #[error("JsEngineError - ScriptSyntax: {0}")]
    ScriptSyntax(String),
    #[error("JsEngineError - ScriptRuntime: {0}")]
    ScriptRuntime(String),
    #[error("JsEngineError - Timeout: script exceeded {0:?} deadline")]
    Timeout(std::time::Duration),
    #[error("JsEngineError - MemoryLimit: script exceeded memory limit")]
    MemoryLimit,
    #[error("JsEngineError - ToolCallLimit: script exceeded {max} tool calls")]
    ToolCallLimit { max: usize },
    #[error("JsEngineError - ToolResultTooLarge: {size} bytes (max {max}). The tool returned more data than compose can hold in a single result. Try a more specific query (e.g. limit/filter parameters) or fall back to call_tool with output_filter.")]
    ToolResultTooLarge { size: usize, max: usize },
    #[error("JsEngineError - ReturnTooLarge: {size} bytes (max {max}). Filter or summarize in the script before returning — compose's job is to shrink large tool results into a small agent-readable answer.")]
    ReturnTooLarge { size: usize, max: usize },
}
