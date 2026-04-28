//! Provider-agnostic LLM backend trait. The prompt executor dispatches via
//! `Arc<dyn LlmProvider>` without knowing the concrete backend.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::stream::StreamDelta;
use crate::{Prompt, PromptError};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// e.g. `"anthropic"`, `"openai"`.
    fn name(&self) -> &str;

    /// Typical event order: content deltas, then `Usage` (additive, may
    /// arrive at any point), then `Done`. Providers emit only the events
    /// natural to their wire format.
    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError>;
}
