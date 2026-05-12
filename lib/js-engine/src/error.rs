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
    #[error("JsEngineError - ToolResultTooLarge: {size} bytes (max {max}). The tool returned more data than compose can hold in a single result. Try a more specific upstream query (limit/filter parameters), or recover the persisted bytes via tool_output_fetch(invocation_id, query) — the universal pipeline already stored the full result.")]
    ToolResultTooLarge { size: usize, max: usize },
    #[error(
        "JsEngineError - ReturnTooLarge: {size} bytes (max {max}). \
         Filter or summarize in the script before returning. If the \
         script's sub-tool calls already produced what you need, look \
         up sub_invocations[].invocation_id from a prior successful \
         compose run and call tool_output_fetch(invocation_id, \
         path, query={{mode:'range'|'lines'|'json_array_slice', offset, len}}) instead of \
         re-running the whole compose script."
    )]
    ReturnTooLarge { size: usize, max: usize },
}
