use tokio::sync::{mpsc, oneshot};

use crate::{Prompt, PromptResponse};

/// A prompt to evaluate, paired with the channel the caller wants the
/// response (with full metadata) written back to.
#[derive(Debug)]
pub struct PromptRequest {
    pub prompt: Prompt,
    pub response_channel: PromptResponseChannel,
}

impl PromptRequest {
    /// Build a request for `prompt`, returning the request to dispatch and the
    /// receiver the caller should `await` to get the response.
    pub fn new(prompt: Prompt) -> (Self, oneshot::Receiver<Result<PromptResponse, PromptError>>) {
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

/// Channel a producer uses to dispatch prompt requests to an evaluator.
pub type PromptRequestChannel = mpsc::Sender<PromptRequest>;

/// One-shot channel an evaluator uses to send the (single) response for a
/// `PromptRequest` back to the originator.
pub type PromptResponseChannel = oneshot::Sender<Result<PromptResponse, PromptError>>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum PromptError {
    #[error("{0}")]
    Provider(String),
}
