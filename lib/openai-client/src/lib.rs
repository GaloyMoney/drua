//! OpenAI Chat Completions client. Accepts provider-agnostic `lib/llm`
//! types at the boundary and yields `StreamDelta` from the SSE stream.

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

/// Retry policy for transient upstream errors (429 / 502 / 503).
/// Capped tight so a wedged provider can't pin the prompt-executor task.
const MAX_RETRIES: u32 = 2;
const MAX_RETRY_AFTER_SECS: u64 = 5;
const DEFAULT_RETRY_DELAY_SECS: u64 = 1;

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

    /// Streams the Chat Completions API and returns the accumulated reply.
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

    #[instrument(name = "openai_client.send_prompt_streaming", skip_all)]
    async fn send_prompt_streaming_internal(
        &self,
        prompt: &Prompt,
    ) -> Result<
        tokio::sync::mpsc::Receiver<Result<StreamDelta, OpenAiChatCompletionsError>>,
        OpenAiChatCompletionsError,
    > {
        let request_body = prompt_to_request(prompt);

        let mut attempt: u32 = 0;
        let resp = loop {
            let resp = self
                .http
                .post(&self.api_url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .json(&request_body)
                .send()
                .await?;

            let status = resp.status();
            if status.is_success() {
                break resp;
            }

            let header_retry = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let message = resp.text().await.unwrap_or_default();
            let is_retryable = matches!(status.as_u16(), 429 | 502 | 503);
            if is_retryable && attempt < MAX_RETRIES {
                let delay = header_retry
                    .or_else(|| parse_retry_after_seconds(&message))
                    .unwrap_or(DEFAULT_RETRY_DELAY_SECS)
                    .min(MAX_RETRY_AFTER_SECS);
                tracing::warn!(
                    attempt = attempt + 1,
                    status = status.as_u16(),
                    delay_secs = delay,
                    "openai-client: retrying after transient HTTP error"
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                attempt += 1;
                continue;
            }

            return Err(OpenAiChatCompletionsError::Api {
                status: status.as_u16(),
                message,
            });
        };

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

/// Best-effort scrape of `error.metadata.retry_after_seconds` from an
/// OpenRouter-style 429 body. Returns None if the body is not JSON, the
/// path is missing, or the value isn't a positive integer.
fn parse_retry_after_seconds(body: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error")?
        .get("metadata")?
        .get("retry_after_seconds")?
        .as_u64()
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

#[cfg(test)]
mod tests {
    use super::parse_retry_after_seconds;

    #[test]
    fn parses_openrouter_429_metadata() {
        let body = r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"...","provider_name":"Together","is_byok":false,"retry_after_seconds":3}},"user_id":"u_x"}"#;
        assert_eq!(parse_retry_after_seconds(body), Some(3));
    }

    #[test]
    fn returns_none_when_metadata_absent() {
        assert_eq!(parse_retry_after_seconds(r#"{"error":{"code":500}}"#), None);
        assert_eq!(parse_retry_after_seconds("not json"), None);
        assert_eq!(parse_retry_after_seconds(""), None);
    }
}
