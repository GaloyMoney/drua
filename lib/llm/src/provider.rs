//! Provider-agnostic LLM backend trait. The prompt executor dispatches via
//! `Arc<dyn LlmProvider>` without knowing the concrete backend.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::stream::StreamDelta;
use crate::{ModelSpec, Prompt, PromptError};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// e.g. `"anthropic"`, `"openai"`.
    fn name(&self) -> &str;

    /// `spec` identifies the chain entry being attempted — providers MUST
    /// read the wire-level model id from `spec.name` and the output cap
    /// from `spec.max_tokens`. `prompt.chain.primary` is metadata about
    /// the originally-requested model and may differ on fallback attempts.
    ///
    /// Typical event order: content deltas, then `Usage` (additive, may
    /// arrive at any point), then `Done`. Providers emit only the events
    /// natural to their wire format.
    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
        spec: &ModelSpec,
    ) -> Result<mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError>;
}
