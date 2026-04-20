//! Provider-agnostic trait for LLM backends.
//!
//! Each provider crate (`anthropic-client`, `openai-client`, …) implements
//! [`LlmProvider`] so that the prompt executor can dispatch to any backend
//! through a single `Arc<dyn LlmProvider>`.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::stream::StreamDelta;
use crate::{Prompt, PromptError};

/// Trait implemented by each LLM provider client.
///
/// The prompt executor stores providers as `Arc<dyn LlmProvider>` and calls
/// [`send_prompt_streaming`] without knowing the concrete backend.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Human-readable provider name (e.g. `"anthropic"`, `"openai"`).
    fn name(&self) -> &str;

    /// Send a prompt and receive streaming deltas via channel.
    ///
    /// The returned receiver yields `StreamDelta` events that conform to the
    /// contract expected by [`crate::stream::StreamAccumulator`]:
    ///
    /// 1. `MessageStart` (with input token count)
    /// 2. One or more content block sequences:
    ///    `ContentBlockStart` → deltas → `ContentBlockStop`
    /// 3. `MessageDelta` (with stop reason and output token count)
    /// 4. `MessageStop`
    ///
    /// Providers that don't natively emit this framing (e.g. OpenAI) must
    /// synthesize the missing events.
    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError>;
}
