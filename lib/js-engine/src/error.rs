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
    #[error("JsEngineError - ResultTooLarge: {size} bytes (max {max})")]
    ResultTooLarge { size: usize, max: usize },
}
