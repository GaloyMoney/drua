//! OpenAI Chat Completions client. Accepts provider-agnostic `lib/llm`
//! types at the boundary and yields `StreamDelta` from the SSE stream.

mod convert;
mod responses;
mod sse;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tracing::{instrument, Instrument};

use llm::provider::LlmProvider;
use llm::stream::StreamDelta;
use llm::{Prompt, PromptError, PromptResponse};

use crate::convert::{prompt_to_request, DeltaSynthesizer, EMPTY_COMPLETION_ERR};
use crate::sse::{parse_sse_stream, SseError};

pub use responses::{OpenAiResponsesAuth, OpenAiResponsesClient, OpenAiResponsesError};

const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const API_PATH: &str = "/v1/chat/completions";

/// Retry policy for transient upstream errors (429 / 502 / 503).
/// Capped tight so a wedged provider can't pin the prompt-executor task.
const MAX_RETRIES: u32 = 2;
const MAX_RETRY_AFTER_SECS: u64 = 5;
const DEFAULT_RETRY_DELAY_SECS: u64 = 1;

/// Maximum number of retries when an upstream returns an empty completion
/// (`finish_reason=stop` with no text and no tool calls). Observed on
/// `deepseek-v4-pro` via OpenRouter, often paired with a flat ~10 s span:
/// the gateway appears to synthesise a stop response when its own provider
/// times out. Retrying once usually succeeds.
const MAX_EMPTY_COMPLETION_RETRIES: u32 = 1;

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

    /// POSTs `body` and waits for response headers. Retries up to
    /// `MAX_RETRIES` times on 429/502/503, honouring `retry-after`. Returns
    /// the response on first 2xx; returns `Api` error on non-retryable or
    /// exhausted retries. Records `http.status_code` and `http.attempts` on
    /// the current span.
    async fn send_request(
        &self,
        body: &[u8],
    ) -> Result<reqwest::Response, OpenAiChatCompletionsError> {
        let span = tracing::Span::current();
        let mut attempt: u32 = 0;
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let resp = self
                .http
                .post(&self.api_url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .body(body.to_vec())
                .send()
                .await?;

            let status = resp.status();
            if status.is_success() {
                span.record("http.status_code", status.as_u16());
                span.record("http.attempts", attempts);
                return Ok(resp);
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

            span.record("http.status_code", status.as_u16());
            span.record("http.attempts", attempts);
            return Err(OpenAiChatCompletionsError::Api {
                status: status.as_u16(),
                message,
            });
        }
    }

    #[instrument(
        name = "openai_client.send_prompt_streaming",
        skip_all,
        fields(
            http.status_code = tracing::field::Empty,
            http.attempts = tracing::field::Empty,
        )
    )]
    async fn send_prompt_streaming_internal(
        &self,
        prompt: &Prompt,
    ) -> Result<
        tokio::sync::mpsc::Receiver<Result<StreamDelta, OpenAiChatCompletionsError>>,
        OpenAiChatCompletionsError,
    > {
        let request_body = prompt_to_request(prompt);
        let body_bytes = Arc::new(
            serde_json::to_vec(&request_body)
                .map_err(|e| OpenAiChatCompletionsError::Stream(format!("body serialize: {e}")))?,
        );

        // Issue first request synchronously so an outright failure surfaces
        // before any receiver is handed out.
        let resp = self.send_request(&body_bytes).await?;

        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<StreamDelta, OpenAiChatCompletionsError>>(128);

        let stream_span = tracing::info_span!(
            parent: tracing::Span::current(),
            "openai_client.stream_processing",
            empty_completion_retries = tracing::field::Empty,
            usage.input_tokens = tracing::field::Empty,
            usage.output_tokens = tracing::field::Empty,
            usage.cache_read_input_tokens = tracing::field::Empty,
            stream.delta_count = tracing::field::Empty,
        );

        let client = self.clone();
        tokio::spawn(
            async move {
                drive_stream_with_retry(client, body_bytes, resp, tx).await;
            }
            .instrument(stream_span),
        );

        Ok(rx)
    }
}

/// Drains the SSE response, forwarding every delta to `tx` in real time.
/// On `EMPTY_COMPLETION_ERR`, re-issues the HTTP request (up to
/// `MAX_EMPTY_COMPLETION_RETRIES` times) and resumes streaming from the
/// new response without forwarding anything from the failed attempt — the
/// synthesizer holds usage internally and discards it on the empty path.
///
/// Records `empty_completion_retries`, `usage.*`, and `stream.delta_count`
/// on `Span::current()` (the `stream_processing` span).
async fn drive_stream_with_retry(
    client: OpenAiClient,
    body: Arc<Vec<u8>>,
    initial_resp: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<Result<StreamDelta, OpenAiChatCompletionsError>>,
) {
    let span = tracing::Span::current();
    let mut current_resp = initial_resp;
    let mut empty_retries: u32 = 0;
    let mut delta_count: u64 = 0;
    let mut usage_seen: Option<(u32, u32, u32)> = None;

    loop {
        let byte_stream = current_resp.bytes_stream();
        let mut synthesizer = DeltaSynthesizer::new();
        let mut empty_completion_seen = false;
        let mut other_processing_err: Option<String> = None;

        let parse_result = parse_sse_stream(byte_stream, |event| {
            match synthesizer.process_chunk(&event.data) {
                Ok(deltas) => {
                    for delta in deltas {
                        delta_count += 1;
                        if let StreamDelta::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            ..
                        } = &delta
                        {
                            usage_seen =
                                Some((*input_tokens, *output_tokens, *cache_read_input_tokens));
                        }
                        tx.try_send(Ok(delta))
                            .map_err(|e| SseError::Processing(e.to_string()))?;
                    }
                    Ok(())
                }
                Err(e) => {
                    if e == EMPTY_COMPLETION_ERR {
                        empty_completion_seen = true;
                    } else {
                        other_processing_err = Some(e.clone());
                    }
                    Err(SseError::Processing(e))
                }
            }
        })
        .await;

        if empty_completion_seen && empty_retries < MAX_EMPTY_COMPLETION_RETRIES {
            empty_retries += 1;
            tracing::warn!(
                retry = empty_retries,
                "openai-client: upstream returned empty completion, retrying"
            );
            match client.send_request(&body).await {
                Ok(new_resp) => {
                    current_resp = new_resp;
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }

        if let Some(msg) = other_processing_err {
            let _ = tx.send(Err(OpenAiChatCompletionsError::Stream(msg))).await;
        } else if empty_completion_seen {
            let _ = tx
                .send(Err(OpenAiChatCompletionsError::Stream(format!(
                    "{EMPTY_COMPLETION_ERR} (after {empty_retries} retries)"
                ))))
                .await;
        } else if let Err(e) = parse_result {
            let _ = tx.send(Err(OpenAiChatCompletionsError::from(e))).await;
        }
        break;
    }

    span.record("empty_completion_retries", empty_retries);
    span.record("stream.delta_count", delta_count);
    if let Some((input, output, cache_read)) = usage_seen {
        span.record("usage.input_tokens", input);
        span.record("usage.output_tokens", output);
        span.record("usage.cache_read_input_tokens", cache_read);
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

/// Status code → classified `PromptError`; transport errors are Transient.
fn classify(err: OpenAiChatCompletionsError) -> PromptError {
    match err {
        OpenAiChatCompletionsError::Http(e) => {
            if e.is_timeout() {
                PromptError::transient(llm::TransientKind::Timeout, e.to_string())
            } else if e.is_connect() {
                PromptError::transient(llm::TransientKind::Connection, e.to_string())
            } else {
                PromptError::transient(llm::TransientKind::ServerError, e.to_string())
            }
        }
        OpenAiChatCompletionsError::Api { status, message } => {
            PromptError::from_http_status(status, message)
        }
        OpenAiChatCompletionsError::Sse(msg) => {
            PromptError::transient(llm::TransientKind::SseDecode, msg)
        }
        OpenAiChatCompletionsError::Stream(msg) => {
            PromptError::transient(llm::TransientKind::ServerError, msg)
        }
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
            .map_err(classify)?;

        let (tx, out_rx) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(result) = rx.recv().await {
                let mapped = result.map_err(classify);
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
