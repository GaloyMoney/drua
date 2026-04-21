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
    /// Events may arrive in any order, but the typical sequence is:
    ///
    /// 1. Content deltas (`TextDelta`, `ThinkingDelta`, `ToolCallStart` /
    ///    `ToolCallDelta`)
    /// 2. `Usage` (token counts — may arrive before, during, or after content;
    ///    accumulated additively)
    /// 3. `Done` (with stop reason)
    ///
    /// Providers need not synthesize lifecycle events — only emit the
    /// content and metadata events that map naturally to their wire format.
    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError>;
}
