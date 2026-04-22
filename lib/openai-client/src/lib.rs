//! OpenAI Chat Completions API client.
//!
//! Accepts the provider-agnostic types from `lib/llm` at the public boundary
//! and uses OpenAI-specific types internally. Streams SSE events and converts
//! them to the unified `StreamDelta` format that the prompt executor and agent
//! loop expect.

mod convert;
mod responses;
mod sse;
mod types;

use async_trait::async_trait;
use thiserror::Error;
use tracing::instrument;

use llm::provider::LlmProvider;
use llm::stream::StreamDelta;
use llm::{Prompt, PromptError, PromptResponse};

use crate::convert::{prompt_to_request, DeltaSynthesizer};
use crate::sse::{parse_sse_stream, SseError};

pub use responses::{OpenAiResponsesAuth, OpenAiResponsesClient, OpenAiResponsesError};

const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const API_PATH: &str = "/v1/chat/completions";

#[derive(Debug, Error)]
pub enum OpenAiChatCompletionsError {
    #[error("OpenAiChatCompletionsError - HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OpenAiChatCompletionsError - API: status={status}, message={message}")]
    Api { status: u16, message: String },
    #[error("OpenAiChatCompletionsError - SSE: {0}")]
    Sse(String),
    #[error("OpenAiChatCompletionsError - Stream: {0}")]
    Stream(String),
}

impl From<SseError> for OpenAiChatCompletionsError {
    fn from(e: SseError) -> Self {
        match e {
            SseError::Http(e) => Self::Http(e),
            SseError::Processing(msg) => Self::Sse(msg),
        }
    }
}

/// OpenAI Chat Completions API client. Converts provider-agnostic `Prompt`
/// values into OpenAI-specific wire types, streams the SSE response, and
/// yields `StreamDelta` events.
#[derive(Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    api_key: String,
    api_url: String,
}

impl OpenAiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            api_url: DEFAULT_API_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(base) = base_url {
            let base = base.trim_end_matches('/');
            self.api_url = format!("{base}{API_PATH}");
        }
        self
    }

    /// Issue a streaming Chat Completions request and return the
    /// fully-accumulated assistant reply.
    #[instrument(name = "openai_client.send_prompt", skip_all)]
    pub async fn send_prompt(
        &self,
        prompt: &Prompt,
    ) -> Result<PromptResponse, OpenAiChatCompletionsError> {
        let rx = self.send_prompt_streaming_internal(prompt).await?;
        let mut rx = rx;

        let mut accumulator = llm::stream::StreamAccumulator::new();
        while let Some(result) = rx.recv().await {
            match result {
                Ok(delta) => accumulator.process(&delta),
                Err(e) => return Err(OpenAiChatCompletionsError::Stream(e.to_string())),
            }
        }
        Ok(accumulator.finish())
    }

    /// Issue a streaming Chat Completions request, yielding provider-agnostic
    /// `StreamDelta`s via channel.
    #[instrument(name = "openai_client.send_prompt_streaming", skip_all)]
    async fn send_prompt_streaming_internal(
        &self,
        prompt: &Prompt,
    ) -> Result<
        tokio::sync::mpsc::Receiver<Result<StreamDelta, OpenAiChatCompletionsError>>,
        OpenAiChatCompletionsError,
    > {
        let request_body = prompt_to_request(prompt);

        let resp = self
            .http
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(OpenAiChatCompletionsError::Api {
                status: status.as_u16(),
                message,
            });
        }

        let byte_stream = resp.bytes_stream();
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<StreamDelta, OpenAiChatCompletionsError>>(128);

        tokio::spawn(async move {
            let tx_ref = &tx;
            let mut synthesizer = DeltaSynthesizer::new();
            let synth_ref = &mut synthesizer;

            let _ = parse_sse_stream(byte_stream, |event| {
                match synth_ref.process_chunk(&event.data) {
                    Ok(deltas) => {
                        for delta in deltas {
                            tx_ref
                                .try_send(Ok(delta))
                                .map_err(|e| SseError::Processing(e.to_string()))?;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        let _ = tx_ref.try_send(Err(OpenAiChatCompletionsError::Stream(e.clone())));
                        Err(SseError::Processing(e))
                    }
                }
            })
            .await;
        });

        Ok(rx)
    }
}

#[async_trait]
impl LlmProvider for OpenAiClient {
    fn name(&self) -> &str {
        "openai"
    }

    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError> {
        let rx = self
            .send_prompt_streaming_internal(prompt)
            .await
            .map_err(|e| PromptError::Provider(e.to_string()))?;

        // Re-map the error type from OpenAiChatCompletionsError to PromptError.
        let (tx, out_rx) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(result) = rx.recv().await {
                let mapped = result.map_err(|e| PromptError::Provider(e.to_string()));
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
        });
        Ok(out_rx)
    }
}
