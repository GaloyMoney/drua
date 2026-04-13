use tokio::sync::mpsc;

use crate::Prompt;

/// A prompt to evaluate, paired with the channel the caller wants the
/// streamed response written back to.
#[derive(Debug)]
pub struct PromptRequest {
    pub prompt: Prompt,
    pub response_channel: PromptResponseChannel,
}

impl PromptRequest {
    /// Build a request for `prompt`, returning the request to dispatch and the
    /// receiver the caller should drain to get streamed response events.
    pub fn new(prompt: Prompt) -> (Self, mpsc::Receiver<PromptResponseEvent>) {
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

/// Channel an evaluator uses to stream response events back to the
/// originator of a `PromptRequest`.
pub type PromptResponseChannel = mpsc::Sender<PromptResponseEvent>;

#[derive(Debug, Clone)]
pub enum PromptResponseEvent {
    Text {
        text: String,
    },
    Done {
        input_tokens: u32,
        output_tokens: u32,
    },
    Error {
        message: String,
    },
}
