use tokio::sync::{mpsc, oneshot};

use crate::stream::StreamDelta;
use crate::{Prompt, PromptResponse};

#[derive(Debug)]
pub struct PromptRequest {
    pub prompt: Prompt,
    pub response_channel: PromptResponseChannel,
}

impl PromptRequest {
    /// Returns the request to dispatch and the response receiver to await.
    pub fn new(prompt: Prompt) -> (Self, oneshot::Receiver<Result<PromptResult, PromptError>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                prompt,
                response_channel: tx,
            },
            rx,
        )
    }
}

pub type PromptRequestChannel = mpsc::Sender<PromptRequest>;
pub type PromptResponseChannel = oneshot::Sender<Result<PromptResult, PromptError>>;

pub struct StreamHandle {
    pub rx: mpsc::Receiver<Result<StreamDelta, PromptError>>,
}

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum PromptResult {
    Complete(PromptResponse),
    Stream(StreamHandle),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PromptError {
    #[error("{0}")]
    Provider(String),
}
