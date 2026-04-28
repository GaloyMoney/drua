//! Anthropic Messages API client. Accepts provider-agnostic `lib/llm` types
//! at the boundary, streams SSE events, and accumulates the response.

mod convert;
mod sse;
mod stream;
mod types;

use async_trait::async_trait;
use thiserror::Error;
use tracing::instrument;

use llm::provider::LlmProvider;
use llm::{Prompt, PromptError, PromptResponse};

use llm::stream::StreamDelta;

use crate::convert::{accumulated_to_response, prompt_to_request, AnthropicDeltaConverter};
use crate::sse::{parse_sse_stream, SseError};
use crate::stream::StreamAccumulator;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_PATH: &str = "/v1/messages";
const API_VERSION: &str = "2023-06-01";

#[derive(Debug, Error)]
pub enum AnthropicError {
    #[error("AnthropicError - HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("AnthropicError - API: status={status}, message={message}")]
    Api { status: u16, message: String },
    #[error("AnthropicError - SSE: {0}")]
    Sse(String),
    #[error("AnthropicError - Stream: {0}")]
    Stream(String),
}

impl From<SseError> for AnthropicError {
    fn from(e: SseError) -> Self {
        match e {
            SseError::Http(e) => Self::Http(e),
            SseError::Processing(msg) => Self::Sse(msg),
        }
    }
}

#[derive(Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    api_url: String,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            api_url: format!("{DEFAULT_BASE_URL}{API_PATH}"),
        }
    }

    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(base) = base_url {
            let base = base.trim_end_matches('/');
            self.api_url = format!("{base}{API_PATH}");
        }
        self
    }

    /// Streams the Messages API and returns the fully-accumulated reply.
    #[instrument(name = "anthropic_client.send_prompt", skip_all)]
    pub async fn send_prompt(&self, prompt: &Prompt) -> Result<PromptResponse, AnthropicError> {
        let request_body = prompt_to_request(prompt);

        let resp = self
            .http
            .post(&self.api_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .json(&request_body)
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

        let byte_stream = resp.bytes_stream();
        let mut accumulator = StreamAccumulator::new();

        let acc_ref = &mut accumulator;
        let sse_result: Result<(), SseError> = parse_sse_stream(byte_stream, |event| {
            if event.event == "ping" {
                return Ok(());
            }
            acc_ref
                .process_event(&event.data)
                .map_err(SseError::Processing)
        })
        .await;

        // SSE-level API errors are captured by the accumulator; only
        // HTTP/transport errors bubble up here.
        if let Err(e) = sse_result {
            if accumulator.is_done() {
                tracing::warn!(error = %e, "SSE stream error after message completed, returning partial response");
            } else {
                return Err(e.into());
            }
        }

        Ok(accumulated_to_response(accumulator.finish()))
    }

    /// Yields provider-agnostic [`StreamDelta`]s via channel; ping events and
    /// events without a delta are filtered out.
    #[instrument(name = "anthropic_client.send_prompt_streaming", skip_all)]
    pub async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamDelta, AnthropicError>>, AnthropicError>
    {
        let request_body = prompt_to_request(prompt);

        let resp = self
            .http
            .post(&self.api_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .json(&request_body)
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

        let byte_stream = resp.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamDelta, AnthropicError>>(128);
        tokio::spawn(async move {
            let tx_ref = &tx;
            let mut converter = AnthropicDeltaConverter::new();
            let converter_ref = &mut converter;
            let _ = parse_sse_stream(byte_stream, |event| {
                if event.event == "ping" {
                    return Ok(());
                }
                match converter_ref.process_event(&event.data) {
                    Ok(deltas) => {
                        for delta in deltas {
                            tx_ref
                                .try_send(Ok(delta))
                                .map_err(|e| SseError::Processing(e.to_string()))?;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        let _ = tx_ref.try_send(Err(AnthropicError::Stream(e.clone())));
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
impl LlmProvider for AnthropicClient {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError> {
        let rx = AnthropicClient::send_prompt_streaming(self, prompt)
            .await
            .map_err(|e| PromptError::Provider(e.to_string()))?;

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
