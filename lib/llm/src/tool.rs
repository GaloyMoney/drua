use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::RequestToolUse;

/// A tool-call request dispatched from the agent loop, paired with the
/// oneshot channel the caller awaits for the result.
#[derive(Debug)]
pub struct ToolUseRequest {
    pub tool_use: RequestToolUse,
    pub response_channel: ToolUseResponseChannel,
}

impl ToolUseRequest {
    /// Build a request for `tool_use`, returning the request to dispatch and
    /// the receiver the caller should `await` to get the result.
    pub fn new(
        tool_use: RequestToolUse,
    ) -> (Self, oneshot::Receiver<Result<ToolUseResult, ToolUseError>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                tool_use,
                response_channel: tx,
            },
            rx,
        )
    }
}

/// Channel a producer (e.g. the `Agents` service) uses to dispatch tool-use
/// requests to a toolset worker.
pub type ToolUseRequestChannel = mpsc::Sender<ToolUseRequest>;

/// One-shot channel a toolset worker uses to send the (single) result for a
/// `ToolUseRequest` back to the originator.
pub type ToolUseResponseChannel = oneshot::Sender<Result<ToolUseResult, ToolUseError>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseResult {
    /// Echo of `RequestToolUse::id` so the caller can pair the result with
    /// the originating tool_use block.
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolUseError {
    #[error("{0}")]
    Provider(String),
}
