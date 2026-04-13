use serde::Deserialize;
use thiserror::Error;
use tracing::instrument;

use llm::prompt::AssistantBlock;
use llm::{Prompt, PromptResponse, StopReason, Usage};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

#[derive(Debug, Error)]
pub enum AnthropicError {
    #[error("AnthropicError - HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("AnthropicError - API: status={status}, message={message}")]
    Api { status: u16, message: String },
}

/// Thin Anthropic Messages API client. Accepts the provider-agnostic types
/// from `lib/llm` and returns a single `PromptResponse` per call. Streaming
/// onto channels is the executor's job — this crate is just the HTTP boundary.
#[derive(Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
        }
    }

    /// Issue a single Messages API request and return the assistant's reply.
    /// The provider-agnostic [`Prompt`] is serialized straight to Anthropic's
    /// wire format — caller is responsible for setting `max_tokens` (the API
    /// requires it).
    #[instrument(name = "anthropic_client.send_prompt", skip_all)]
    pub async fn send_prompt(&self, prompt: &Prompt) -> Result<PromptResponse, AnthropicError> {
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(prompt)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(AnthropicError::Api {
                status: status.as_u16(),
                message,
            });
        }

        let wire: WireResponse = resp.json().await?;
        Ok(PromptResponse {
            content: wire.content,
            usage: wire.usage,
            stop_reason: wire.stop_reason,
        })
    }
}

/// Subset of Anthropic's `messages` response we care about; ignores the
/// envelope fields (`id`, `type`, `role`, `model`, `stop_sequence`).
#[derive(Deserialize)]
struct WireResponse {
    content: Vec<AssistantBlock>,
    usage: Usage,
    #[serde(default)]
    stop_reason: Option<StopReason>,
}
