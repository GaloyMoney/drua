//! Anthropic Messages API client.
//!
//! Accepts the provider-agnostic types from `lib/llm` at the public boundary
//! and uses Anthropic-specific types (ported from the Pi agent crate)
//! internally. Streams SSE events and accumulates the response before
//! returning a single `PromptResponse`.

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

use crate::convert::{accumulated_to_response, prompt_to_request, sse_data_to_delta};
use crate::sse::{parse_sse_stream, SseError};
use crate::stream::StreamAccumulator;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
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

/// Anthropic Messages API client. Converts provider-agnostic `Prompt` values
/// into Anthropic-specific wire types, streams the SSE response, and
/// accumulates the result into a single `PromptResponse`.
///
/// Internally uses types ported from the Pi agent crate. The public interface
/// (`new`, `send_prompt`) remains unchanged so callers need no modifications.
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

    /// Issue a streaming Messages API request and return the fully-accumulated
    /// assistant reply.
    ///
    /// Internally converts the provider-agnostic [`Prompt`] to Anthropic's
    /// wire format, sends a streaming request, parses SSE events, and
    /// accumulates text/tool-use/thinking blocks into a single
    /// [`PromptResponse`].
    #[instrument(name = "anthropic_client.send_prompt", skip_all)]
    pub async fn send_prompt(&self, prompt: &Prompt) -> Result<PromptResponse, AnthropicError> {
        let request_body = prompt_to_request(prompt);

        let resp = self
            .http
            .post(API_URL)
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

        // Stream SSE events and accumulate the response.
        let byte_stream = resp.bytes_stream();
        let mut accumulator = StreamAccumulator::new();

        // We need to move the accumulator into the closure but also get it
        // back after parsing completes. Use a mutable reference captured by
        // the closure — `parse_sse_stream` takes `FnMut`.
        let acc_ref = &mut accumulator;
        let sse_result: Result<(), SseError> = parse_sse_stream(byte_stream, |event| {
            // Skip ping events (keep-alive).
            if event.event == "ping" {
                return Ok(());
            }
            acc_ref
                .process_event(&event.data)
                .map_err(SseError::Processing)
        })
        .await;

        // An SSE-level error from the API (e.g. `{"type":"error","error":{...}}`)
        // is captured by the accumulator; HTTP/transport errors bubble up here.
        if let Err(e) = sse_result {
            // If the accumulator already captured partial content and the stream
            // simply ended, return what we have. Otherwise propagate the error.
            if accumulator.is_done() {
                tracing::warn!(error = %e, "SSE stream error after message completed, returning partial response");
            } else {
                return Err(e.into());
            }
        }

        Ok(accumulated_to_response(accumulator.finish()))
    }

    /// Issue a streaming Messages API request, yielding provider-agnostic
    /// [`StreamDelta`]s via channel. Ping events and events that carry no
    /// delta are filtered out. The Anthropic→`StreamDelta` conversion
    /// happens inside this method so callers never see Anthropic wire types.
    #[instrument(name = "anthropic_client.send_prompt_streaming", skip_all)]
    pub async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamDelta, AnthropicError>>, AnthropicError>
    {
        let request_body = prompt_to_request(prompt);

        let resp = self
            .http
            .post(API_URL)
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
            let _ = parse_sse_stream(byte_stream, |event| {
                if event.event == "ping" {
                    return Ok(());
                }
                match sse_data_to_delta(&event.data) {
                    Ok(Some(delta)) => tx_ref
                        .try_send(Ok(delta))
                        .map_err(|e| SseError::Processing(e.to_string())),
                    Ok(None) => Ok(()),
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

        // Re-map the error type from AnthropicError to PromptError.
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
