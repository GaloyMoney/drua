use tokio::sync::mpsc;

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
    /// receiver the caller should drain to get response batches.
    pub fn new(
        prompt: Prompt,
    ) -> (Self, mpsc::Receiver<Result<PromptResponse, PromptError>>) {
        let (tx, rx) = mpsc::channel(64);
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

/// Channel an evaluator uses to stream responses back to the originator of a
/// `PromptRequest`. Each send delivers one full `PromptResponse` (content +
/// usage + stop_reason) or an error.
pub type PromptResponseChannel = mpsc::Sender<Result<PromptResponse, PromptError>>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum PromptError {
    #[error("{0}")]
    Provider(String),
}
