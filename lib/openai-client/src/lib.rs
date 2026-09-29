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

use crate::convert::{prompt_to_request, DeltaSynthesizer, ReasoningDialect, EMPTY_COMPLETION_ERR};
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

/// Maximum number of retries when the SSE byte stream itself breaks
/// (connection drop, body-decode failure) before any delta from that
/// attempt reached the consumer — safe to retry since nothing has leaked
/// into the caller's session log yet. See
/// `review-curation-live-run4-2026-09-28.md` R1: previously a model chain
/// with a single entry had no recovery from a mid-stream transient error
/// at all, once past the first request.
const MAX_TRANSIENT_STREAM_RETRIES: u32 = 1;

fn reasoning_dialect_for_base_url(base_url: &str) -> ReasoningDialect {
    if base_url.to_ascii_lowercase().contains("openrouter.ai") {
        ReasoningDialect::OpenRouter
    } else {
        ReasoningDialect::OpenAi
    }
}

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
    reasoning_dialect: ReasoningDialect,
}

impl OpenAiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            api_url: DEFAULT_API_URL.to_string(),
            reasoning_dialect: ReasoningDialect::OpenAi,
        }
    }

    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(base) = base_url {
            self.reasoning_dialect = reasoning_dialect_for_base_url(&base);
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
            agent_id = tracing::field::Empty,
            run_id = tracing::field::Empty,
            model_used = tracing::field::Empty,
        )
    )]
    async fn send_prompt_streaming_internal(
        &self,
        prompt: &Prompt,
    ) -> Result<
        tokio::sync::mpsc::Receiver<Result<StreamDelta, OpenAiChatCompletionsError>>,
        OpenAiChatCompletionsError,
    > {
        // R5(a), review-curation-live-run4-2026-09-28.md: without these,
        // an environment-wide Honeycomb query over this span mixes every
        // concurrent agent/workflow-run's turns together. `trace_agent_id`
        // / `trace_run_id` are plumbed through from `Agents::drive_session_loop`
        // and never sent to the provider (see `llm::Prompt`'s doc comment).
        let span = tracing::Span::current();
        if let Some(agent_id) = &prompt.trace_agent_id {
            span.record("agent_id", agent_id.as_str());
        }
        if let Some(run_id) = &prompt.trace_run_id {
            span.record("run_id", run_id.as_str());
        }
        span.record("model_used", prompt.chain.primary.name.as_str());

        let request_body = prompt_to_request(prompt, self.reasoning_dialect);
        let body_bytes = Arc::new(
            serde_json::to_vec(&request_body)
                .map_err(|e| OpenAiChatCompletionsError::Stream(format!("body serialize: {e}")))?,
        );

        // Issue first request synchronously so an outright failure surfaces
        // before any receiver is handed out. Timed from here (not from
        // function entry) so `ttfb_ms` reflects wire time, not queueing
        // ahead of the request.
        let request_start = std::time::Instant::now();
        let resp = self.send_request(&body_bytes).await?;

        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<StreamDelta, OpenAiChatCompletionsError>>(128);

        let stream_span = tracing::info_span!(
            parent: tracing::Span::current(),
            "openai_client.stream_processing",
            agent_id = tracing::field::Empty,
            run_id = tracing::field::Empty,
            model_used = tracing::field::Empty,
            ttfb_ms = tracing::field::Empty,
            empty_completion_retries = tracing::field::Empty,
            transient_stream_retries = tracing::field::Empty,
            usage.input_tokens = tracing::field::Empty,
            usage.output_tokens = tracing::field::Empty,
            usage.cache_read_input_tokens = tracing::field::Empty,
            usage.cache_creation_input_tokens = tracing::field::Empty,
            usage.reasoning_output_tokens = tracing::field::Empty,
            usage.cost_usd = tracing::field::Empty,
            usage.upstream_inference_cost_usd = tracing::field::Empty,
            upstream_provider = tracing::field::Empty,
            stream.delta_count = tracing::field::Empty,
        );
        if let Some(agent_id) = &prompt.trace_agent_id {
            stream_span.record("agent_id", agent_id.as_str());
        }
        if let Some(run_id) = &prompt.trace_run_id {
            stream_span.record("run_id", run_id.as_str());
        }
        stream_span.record("model_used", prompt.chain.primary.name.as_str());

        let client = self.clone();
        tokio::spawn(
            async move {
                drive_stream_with_retry(client, body_bytes, resp, tx, request_start).await;
            }
            .instrument(stream_span),
        );

        Ok(rx)
    }
}

/// Drains the SSE response, forwarding every delta to `tx` in real time.
///
/// On `EMPTY_COMPLETION_ERR`, re-issues the HTTP request (up to
/// `MAX_EMPTY_COMPLETION_RETRIES` times) and resumes streaming from the
/// new response without forwarding anything from the failed attempt — the
/// synthesizer holds usage internally and discards it on the empty path.
///
/// On a mid-stream transport failure (`SseError::Http` — connection drop,
/// body-decode error) that happened before this attempt forwarded any
/// delta to `tx`, re-issues the request the same way (up to
/// `MAX_TRANSIENT_STREAM_RETRIES` times). Gated on "no delta forwarded
/// yet" for the same reason as `router::walk`'s fallback is gated on
/// never having opened a stream: once a delta has reached the consumer,
/// retrying would duplicate it there — see `review-curation-live-run4-2026-09-28.md`
/// R1.
///
/// Records `empty_completion_retries`, `transient_stream_retries`,
/// `usage.*`, and `stream.delta_count` on `Span::current()` (the
/// `stream_processing` span).
async fn drive_stream_with_retry(
    client: OpenAiClient,
    body: Arc<Vec<u8>>,
    initial_resp: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<Result<StreamDelta, OpenAiChatCompletionsError>>,
    request_start: std::time::Instant,
) {
    let span = tracing::Span::current();
    let mut current_resp = initial_resp;
    let mut empty_retries: u32 = 0;
    let mut transient_retries: u32 = 0;
    let mut delta_count: u64 = 0;
    let mut usage_seen: Option<UsageSnapshot> = None;
    let mut ttfb_recorded = false;

    loop {
        let byte_stream = current_resp.bytes_stream();
        let mut synthesizer = DeltaSynthesizer::new();
        let mut empty_completion_seen = false;
        let mut other_processing_err: Option<String> = None;
        let deltas_before_attempt = delta_count;

        let parse_result = parse_sse_stream(byte_stream, |event| {
            match synthesizer.process_chunk(&event.data) {
                Ok(deltas) => {
                    for delta in deltas {
                        delta_count += 1;
                        if !ttfb_recorded {
                            ttfb_recorded = true;
                            span.record("ttfb_ms", request_start.elapsed().as_millis() as u64);
                        }
                        if let StreamDelta::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                            reasoning_output_tokens,
                            cost_usd,
                            upstream_inference_cost_usd,
                            upstream_provider,
                        } = &delta
                        {
                            usage_seen = Some(UsageSnapshot {
                                input_tokens: *input_tokens,
                                output_tokens: *output_tokens,
                                cache_read_input_tokens: *cache_read_input_tokens,
                                cache_creation_input_tokens: *cache_creation_input_tokens,
                                reasoning_output_tokens: *reasoning_output_tokens,
                                cost_usd: *cost_usd,
                                upstream_inference_cost_usd: *upstream_inference_cost_usd,
                                upstream_provider: upstream_provider.clone(),
                            });
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

        let forwarded_this_attempt = delta_count > deltas_before_attempt;
        if should_retry_transient_stream_error(
            &parse_result,
            forwarded_this_attempt,
            transient_retries,
        ) {
            transient_retries += 1;
            let error = parse_result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            tracing::warn!(
                retry = transient_retries,
                %error,
                "openai-client: transient stream error before any delta was forwarded, retrying"
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
    span.record("transient_stream_retries", transient_retries);
    span.record("stream.delta_count", delta_count);
    if let Some(u) = usage_seen {
        span.record("usage.input_tokens", u.input_tokens);
        span.record("usage.output_tokens", u.output_tokens);
        span.record("usage.cache_read_input_tokens", u.cache_read_input_tokens);
        span.record(
            "usage.cache_creation_input_tokens",
            u.cache_creation_input_tokens,
        );
        span.record("usage.reasoning_output_tokens", u.reasoning_output_tokens);
        if let Some(c) = u.cost_usd {
            span.record("usage.cost_usd", c);
        }
        if let Some(c) = u.upstream_inference_cost_usd {
            span.record("usage.upstream_inference_cost_usd", c);
        }
        if let Some(p) = &u.upstream_provider {
            span.record("upstream_provider", p.as_str());
        }
    }
}

#[derive(Clone)]
struct UsageSnapshot {
    input_tokens: u32,
    output_tokens: u32,
    cache_read_input_tokens: u32,
    cache_creation_input_tokens: u32,
    reasoning_output_tokens: u32,
    cost_usd: Option<f64>,
    upstream_inference_cost_usd: Option<f64>,
    upstream_provider: Option<String>,
}

/// Gate for the mid-stream transient-error retry in
/// `drive_stream_with_retry`: only a transport-level failure
/// (`SseError::Http` — connection drop, body-decode error), only before
/// this attempt has forwarded anything to the consumer, and only within
/// the retry budget. A `SseError::Processing` failure (malformed event,
/// or the consumer channel itself closed) is never retried here — the
/// former may have already forwarded partial content that a retry would
/// duplicate, the latter means retrying is pointless.
fn should_retry_transient_stream_error(
    parse_result: &Result<(), SseError>,
    forwarded_this_attempt: bool,
    retries_so_far: u32,
) -> bool {
    !forwarded_this_attempt
        && retries_so_far < MAX_TRANSIENT_STREAM_RETRIES
        && matches!(parse_result, Err(SseError::Http(_)))
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
    use super::{
        parse_retry_after_seconds, reasoning_dialect_for_base_url,
        should_retry_transient_stream_error, ReasoningDialect, SseError,
        MAX_TRANSIENT_STREAM_RETRIES,
    };

    /// A genuine `reqwest::Error` (connection refused — no real network
    /// needed, nothing listens on the loopback port) to build a real
    /// `SseError::Http`. `reqwest::Error` has no public constructor, so
    /// this is the only reliable way to get one for a test.
    async fn connection_refused_error() -> reqwest::Error {
        reqwest::get("http://127.0.0.1:1/")
            .await
            .expect_err("nothing listens on 127.0.0.1:1")
    }

    #[tokio::test]
    async fn transient_stream_retry_allowed_before_any_delta_forwarded() {
        let result: Result<(), SseError> = Err(SseError::Http(connection_refused_error().await));
        assert!(should_retry_transient_stream_error(&result, false, 0));
    }

    #[tokio::test]
    async fn transient_stream_retry_refused_once_a_delta_already_forwarded() {
        // A retry here would duplicate content already handed to the
        // consumer — must never happen, regardless of budget.
        let result: Result<(), SseError> = Err(SseError::Http(connection_refused_error().await));
        assert!(!should_retry_transient_stream_error(&result, true, 0));
    }

    #[tokio::test]
    async fn transient_stream_retry_refused_once_budget_exhausted() {
        let result: Result<(), SseError> = Err(SseError::Http(connection_refused_error().await));
        assert!(!should_retry_transient_stream_error(
            &result,
            false,
            MAX_TRANSIENT_STREAM_RETRIES
        ));
    }

    #[test]
    fn transient_stream_retry_refused_for_processing_errors() {
        // A decode/processing error (as opposed to a transport failure)
        // is not retried by this path — see EMPTY_COMPLETION_ERR's own
        // dedicated retry for the one processing error that IS retried.
        let result: Result<(), SseError> = Err(SseError::Processing("bad json".to_string()));
        assert!(!should_retry_transient_stream_error(&result, false, 0));
    }

    #[test]
    fn transient_stream_retry_refused_when_there_was_no_error() {
        let result: Result<(), SseError> = Ok(());
        assert!(!should_retry_transient_stream_error(&result, false, 0));
    }

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

    #[test]
    fn reasoning_dialect_detects_openrouter() {
        assert_eq!(
            reasoning_dialect_for_base_url("https://openrouter.ai/api"),
            ReasoningDialect::OpenRouter
        );
        assert_eq!(
            reasoning_dialect_for_base_url("https://api.openai.com"),
            ReasoningDialect::OpenAi
        );
    }
}
