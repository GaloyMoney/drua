//! HTTP client for TypeSafe Jev and its `/v1/systemone`-compatible clones
//! ("decision models"): a plain non-streaming JSON POST that takes text or
//! JSON state plus closed-set questions and returns calibrated
//! probabilities in one forward pass. See
//! `handoff-decide-tool-and-step-2026-10-05.md` for the full design.
//!
//! Deliberately not built on `lib/openai-client` — that client is
//! hardwired to `/v1/chat/completions`, SSE streaming, and `Prompt` → chat
//! conversion. A decision call has a different body and no streaming; only
//! the retry loop is copied (`OpenAiClient::send_request`).

mod types;

use std::time::Duration;

use thiserror::Error;
use tracing::{instrument, Span};

pub use types::{Answer, DecisionRequest, DecisionResponse, DecisionUsage, NoulCriteria, Question};

/// Retry policy for transient upstream errors, copied from
/// `lib/openai-client`'s `OpenAiClient::send_request`.
const MAX_RETRIES: u32 = 2;
const MAX_RETRY_AFTER_SECS: u64 = 5;
const DEFAULT_RETRY_DELAY_SECS: u64 = 1;

/// Per-deployment ceilings on a `decide` request, enforced by [`validate`]
/// before any HTTP call. Defaults: 64 questions, 131_072 bytes of
/// serialized `state` (≈32k tokens, the model's state cap).
#[derive(Debug, Clone, Copy)]
pub struct DecisionLimits {
    pub max_questions: usize,
    pub max_state_bytes: usize,
}

impl Default for DecisionLimits {
    fn default() -> Self {
        Self {
            max_questions: 64,
            max_state_bytes: 131_072,
        }
    }
}

#[derive(Debug, Error)]
pub enum DecisionClientError {
    #[error("DecisionClientError - Invalid: {0}")]
    Invalid(String),
    #[error("DecisionClientError - HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("DecisionClientError - API: status={status}, message={message}")]
    Api { status: u16, message: String },
    #[error("DecisionClientError - Decode: {0}")]
    Decode(String),
}

#[derive(Clone)]
pub struct DecisionClient {
    http: reqwest::Client,
    api_key: String,
    endpoint_url: String,
    limits: DecisionLimits,
}

impl DecisionClient {
    /// `endpoint_url` is the full URL — OpenRouter
    /// (`https://openrouter.ai/api/alpha/decisions`), TypeSafe direct
    /// (`https://api.typesafe.ai/v1/systemone`), or a self-hosted Clef
    /// instance. The three providers disagree on the path, so callers
    /// must not compose one from a base URL.
    pub fn new(
        api_key: impl Into<String>,
        endpoint_url: impl Into<String>,
        timeout: Duration,
        limits: DecisionLimits,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("reqwest client"),
            api_key: api_key.into(),
            endpoint_url: endpoint_url.into(),
            limits,
        }
    }

    pub fn limits(&self) -> &DecisionLimits {
        &self.limits
    }

    #[instrument(
        name = "decision_client.decide",
        skip_all,
        fields(
            http.status_code = tracing::field::Empty,
            http.attempts = tracing::field::Empty,
            model = tracing::field::Empty,
            question_count = tracing::field::Empty,
        )
    )]
    pub async fn decide(
        &self,
        request: &DecisionRequest,
    ) -> Result<DecisionResponse, DecisionClientError> {
        let span = Span::current();
        span.record("model", request.model.as_str());
        span.record("question_count", request.questions.len());

        let body = serde_json::to_vec(request)
            .map_err(|e| DecisionClientError::Invalid(format!("body serialize: {e}")))?;
        let resp = self.send_request(&body).await?;
        let bytes = resp.bytes().await.map_err(DecisionClientError::Http)?;
        let decoded: DecisionResponse = serde_json::from_slice(&bytes).map_err(|e| {
            let snippet_len = bytes.len().min(512);
            let snippet = String::from_utf8_lossy(&bytes[..snippet_len]);
            DecisionClientError::Decode(format!("{e}: {snippet}"))
        })?;

        for key in request.questions.keys() {
            if !decoded.answers.contains_key(key) {
                return Err(DecisionClientError::Decode(format!(
                    "answer for `{key}` missing"
                )));
            }
        }

        Ok(decoded)
    }

    /// POSTs `body` and waits for response headers. Retries up to
    /// `MAX_RETRIES` times on 429/502/503/529 (529 = provider overloaded,
    /// documented for this endpoint), honouring `retry-after`. Records
    /// `http.status_code` and `http.attempts` on the current span.
    async fn send_request(&self, body: &[u8]) -> Result<reqwest::Response, DecisionClientError> {
        let span = Span::current();
        let mut attempt: u32 = 0;
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let resp = self
                .http
                .post(&self.endpoint_url)
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
            let is_retryable = matches!(status.as_u16(), 429 | 502 | 503 | 529);
            if is_retryable && attempt < MAX_RETRIES {
                let delay = header_retry
                    .or_else(|| parse_retry_after_seconds(&message))
                    .unwrap_or(DEFAULT_RETRY_DELAY_SECS)
                    .min(MAX_RETRY_AFTER_SECS);
                tracing::warn!(
                    attempt = attempt + 1,
                    status = status.as_u16(),
                    delay_secs = delay,
                    "decision-client: retrying after transient HTTP error"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                attempt += 1;
                continue;
            }

            span.record("http.status_code", status.as_u16());
            span.record("http.attempts", attempts);
            return Err(DecisionClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
    }
}

/// Best-effort scrape of `error.metadata.retry_after_seconds` from an
/// OpenRouter-style error body. Returns `None` if the body is not JSON,
/// the path is missing, or the value isn't a positive integer. Copied
/// from `lib/openai-client`'s `parse_retry_after_seconds`.
fn parse_retry_after_seconds(body: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error")?
        .get("metadata")?
        .get("retry_after_seconds")?
        .as_u64()
}

fn is_valid_question_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn check_instructions(key: &str, instructions: &str) -> Result<(), DecisionClientError> {
    if instructions.trim().is_empty() {
        return Err(DecisionClientError::Invalid(format!(
            "question '{key}': instructions must not be empty"
        )));
    }
    Ok(())
}

/// Validates question shape independent of `state` — so a workflow-step
/// parser can check `questions` before it has a `state` to substitute.
/// Pure; runs before any HTTP call.
pub fn validate_questions(
    questions: &std::collections::BTreeMap<String, Question>,
    limits: &DecisionLimits,
) -> Result<(), DecisionClientError> {
    if questions.is_empty() || questions.len() > limits.max_questions {
        return Err(DecisionClientError::Invalid(format!(
            "questions: must have 1..={} entries, got {}",
            limits.max_questions,
            questions.len()
        )));
    }
    for (key, question) in questions {
        if !is_valid_question_key(key) {
            return Err(DecisionClientError::Invalid(format!(
                "question key '{key}' must match ^[A-Za-z_][A-Za-z0-9_]*$"
            )));
        }
        match question {
            Question::Noul { instructions, .. } => check_instructions(key, instructions)?,
            Question::Choice {
                instructions,
                criteria,
            } => {
                check_instructions(key, instructions)?;
                if criteria.len() < 2 || criteria.len() > 255 {
                    return Err(DecisionClientError::Invalid(format!(
                        "question '{key}': choice must have 2..=255 options, got {}",
                        criteria.len()
                    )));
                }
                if criteria.keys().any(|k| k.is_empty()) {
                    return Err(DecisionClientError::Invalid(format!(
                        "question '{key}': choice option keys must be non-empty"
                    )));
                }
            }
            Question::Score {
                instructions,
                criteria,
            } => {
                check_instructions(key, instructions)?;
                if criteria.len() < 2 || criteria.len() > 10 {
                    return Err(DecisionClientError::Invalid(format!(
                        "question '{key}': score must have 2..=10 levels, got {}",
                        criteria.len()
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Validates `state`'s serialized size against `limits` — split out from
/// [`validate_questions`] so the two can be checked independently (the
/// workflow step validates `questions` at parse time, before `state` has
/// been `${{ … }}`-substituted).
pub fn validate_state_size(
    state: &serde_json::Value,
    limits: &DecisionLimits,
) -> Result<(), DecisionClientError> {
    let len = serde_json::to_vec(state)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if len > limits.max_state_bytes {
        return Err(DecisionClientError::Invalid(format!(
            "state: {len} bytes exceeds max_state_bytes {}",
            limits.max_state_bytes
        )));
    }
    Ok(())
}

/// Full pre-flight validation of a request: `questions` then `state`.
/// Pure, so the workflow validator can reuse it without a client.
pub fn validate(
    request: &DecisionRequest,
    limits: &DecisionLimits,
) -> Result<(), DecisionClientError> {
    validate_questions(&request.questions, limits)?;
    validate_state_size(&request.state, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn noul(instructions: &str) -> Question {
        Question::Noul {
            instructions: instructions.to_string(),
            criteria: None,
        }
    }

    fn one_question() -> BTreeMap<String, Question> {
        let mut m = BTreeMap::new();
        m.insert("ok".to_string(), noul("Is this fine?"));
        m
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
    fn validate_accepts_one_valid_question() {
        let limits = DecisionLimits::default();
        assert!(validate_questions(&one_question(), &limits).is_ok());
        assert!(validate_state_size(&serde_json::json!("hello"), &limits).is_ok());
    }

    #[test]
    fn validate_rejects_zero_questions() {
        let limits = DecisionLimits::default();
        assert!(validate_questions(&BTreeMap::new(), &limits).is_err());
    }

    #[test]
    fn validate_rejects_too_many_questions() {
        let limits = DecisionLimits {
            max_questions: 2,
            ..Default::default()
        };
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), noul("a?"));
        m.insert("b".to_string(), noul("b?"));
        m.insert("c".to_string(), noul("c?"));
        assert!(validate_questions(&m, &limits).is_err());
    }

    #[test]
    fn validate_rejects_bad_key() {
        let limits = DecisionLimits::default();
        let mut m = BTreeMap::new();
        m.insert("bad-key".to_string(), noul("ok?"));
        assert!(validate_questions(&m, &limits).is_err());
        let mut m2 = BTreeMap::new();
        m2.insert("1bad".to_string(), noul("ok?"));
        assert!(validate_questions(&m2, &limits).is_err());
    }

    #[test]
    fn validate_rejects_empty_instructions() {
        let limits = DecisionLimits::default();
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), noul("   "));
        assert!(validate_questions(&m, &limits).is_err());
    }

    #[test]
    fn validate_rejects_choice_with_one_option() {
        let limits = DecisionLimits::default();
        let mut criteria = BTreeMap::new();
        criteria.insert("only".to_string(), "the only option".to_string());
        let mut m = BTreeMap::new();
        m.insert(
            "k".to_string(),
            Question::Choice {
                instructions: "pick one".into(),
                criteria,
            },
        );
        assert!(validate_questions(&m, &limits).is_err());
    }

    #[test]
    fn validate_accepts_choice_with_two_options() {
        let limits = DecisionLimits::default();
        let mut criteria = BTreeMap::new();
        criteria.insert("a".to_string(), "A".to_string());
        criteria.insert("b".to_string(), "B".to_string());
        let mut m = BTreeMap::new();
        m.insert(
            "k".to_string(),
            Question::Choice {
                instructions: "pick one".into(),
                criteria,
            },
        );
        assert!(validate_questions(&m, &limits).is_ok());
    }

    #[test]
    fn validate_rejects_score_with_one_level() {
        let limits = DecisionLimits::default();
        let mut m = BTreeMap::new();
        m.insert(
            "k".to_string(),
            Question::Score {
                instructions: "how much".into(),
                criteria: vec!["only".into()],
            },
        );
        assert!(validate_questions(&m, &limits).is_err());
    }

    #[test]
    fn validate_rejects_score_with_eleven_levels() {
        let limits = DecisionLimits::default();
        let m = BTreeMap::from([(
            "k".to_string(),
            Question::Score {
                instructions: "how much".into(),
                criteria: (0..11).map(|n| n.to_string()).collect(),
            },
        )]);
        assert!(validate_questions(&m, &limits).is_err());
    }

    #[test]
    fn validate_rejects_state_over_byte_limit() {
        let limits = DecisionLimits {
            max_state_bytes: 4,
            ..Default::default()
        };
        assert!(validate_state_size(&serde_json::json!("too long"), &limits).is_err());
    }

    #[test]
    fn validate_full_request_checks_both_questions_and_state() {
        let limits = DecisionLimits::default();
        let req = DecisionRequest {
            model: "typesafe/jev-1.13".into(),
            state: serde_json::json!("hello"),
            questions: one_question(),
        };
        assert!(validate(&req, &limits).is_ok());
    }

    // ── In-process fake server ──

    mod fake_server {
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        #[derive(Clone)]
        pub struct Captured {
            pub attempts: Arc<AtomicU32>,
            pub last_auth: Arc<std::sync::Mutex<Option<String>>>,
            pub last_body: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
        }

        type RespondFn =
            dyn Fn(u32) -> (StatusCode, Option<&'static str>, serde_json::Value) + Send + Sync;

        /// Spawns a fake decisions endpoint on an ephemeral loopback port.
        /// `respond` sees the 1-based attempt number and returns
        /// `(status, retry_after_header, json_body)`.
        pub async fn spawn(
            respond: impl Fn(u32) -> (StatusCode, Option<&'static str>, serde_json::Value)
                + Send
                + Sync
                + 'static,
        ) -> (String, Captured) {
            let captured = Captured {
                attempts: Arc::new(AtomicU32::new(0)),
                last_auth: Arc::new(std::sync::Mutex::new(None)),
                last_body: Arc::new(std::sync::Mutex::new(None)),
            };
            let respond: Arc<RespondFn> = Arc::new(respond);

            #[derive(Clone)]
            struct AppState {
                captured: Captured,
                respond: Arc<RespondFn>,
            }

            async fn handler(
                State(state): State<AppState>,
                headers: HeaderMap,
                Json(body): Json<serde_json::Value>,
            ) -> (StatusCode, HeaderMap, Json<serde_json::Value>) {
                let attempt = state.captured.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                *state.captured.last_auth.lock().unwrap() = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                *state.captured.last_body.lock().unwrap() = Some(body);

                let (status, retry_after, json_body) = (state.respond)(attempt);
                let mut out_headers = HeaderMap::new();
                if let Some(ra) = retry_after {
                    out_headers.insert("retry-after", ra.parse().unwrap());
                }
                (status, out_headers, Json(json_body))
            }

            let state = AppState {
                captured: captured.clone(),
                respond,
            };
            let app = Router::new()
                .route("/decisions", post(handler))
                .with_state(state);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            (format!("http://{addr}/decisions"), captured)
        }
    }

    fn fake_decision_response() -> serde_json::Value {
        serde_json::json!({
            "model": "typesafe/jev-1.13",
            "answers": { "ok": { "type": "noul", "noul": 0.9 } },
            "usage": { "input_tokens": 10, "output_tokens": 1, "cost_usd": 0.000001 }
        })
    }

    #[tokio::test]
    async fn decide_retries_once_on_429_then_succeeds() {
        let (url, captured) = fake_server::spawn(|attempt| {
            if attempt == 1 {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    Some("0"),
                    serde_json::json!({"error": "rate limited"}),
                )
            } else {
                (axum::http::StatusCode::OK, None, fake_decision_response())
            }
        })
        .await;

        let client = DecisionClient::new(
            "test-key",
            url,
            Duration::from_secs(5),
            DecisionLimits::default(),
        );
        let request = DecisionRequest {
            model: "typesafe/jev-1.13".into(),
            state: serde_json::json!("some state"),
            questions: one_question(),
        };
        let resp = client.decide(&request).await.expect("should succeed");
        assert!((resp.answers["ok"].confidence_floor() - 0.9).abs() < 1e-9);
        assert_eq!(
            captured.attempts.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            captured.last_auth.lock().unwrap().as_deref(),
            Some("Bearer test-key")
        );
        let body = captured.last_body.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], "typesafe/jev-1.13");
        assert!(body["questions"]["ok"].is_object());
    }

    #[tokio::test]
    async fn decide_exhausts_retries_on_repeated_529() {
        let (url, captured) = fake_server::spawn(|_attempt| {
            (
                axum::http::StatusCode::from_u16(529).unwrap(),
                None,
                serde_json::json!({"error": "overloaded"}),
            )
        })
        .await;

        let client = DecisionClient::new(
            "test-key",
            url,
            Duration::from_secs(5),
            DecisionLimits::default(),
        );
        let request = DecisionRequest {
            model: "typesafe/jev-1.13".into(),
            state: serde_json::json!("some state"),
            questions: one_question(),
        };
        let err = client.decide(&request).await.expect_err("should fail");
        assert!(matches!(err, DecisionClientError::Api { status: 529, .. }));
        assert_eq!(
            captured.attempts.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
    }
}
