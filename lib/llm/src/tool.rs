use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::RequestToolUse;

#[derive(Debug)]
pub struct ToolUseRequest {
    pub tool_use: RequestToolUse,
    pub response_channel: ToolUseResponseChannel,
}

impl ToolUseRequest {
    /// Returns the request to dispatch and the receiver to await.
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

pub type ToolUseRequestChannel = mpsc::Sender<ToolUseRequest>;
pub type ToolUseResponseChannel = oneshot::Sender<Result<ToolUseResult, ToolUseError>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseResult {
    /// Echoes `RequestToolUse::id` for pairing with the originating tool_use.
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolUseError {
    #[error("{0}")]
    Provider(String),
}
