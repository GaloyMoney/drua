use thiserror::Error;

/// Errors returned by the Zenduty client.
#[derive(Debug, Error)]
pub enum ZendutyError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("failed to parse URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid header value: {0}")]
    InvalidHeader(String),

    #[error("unauthorized — check ZENDUTY_API_TOKEN: {0}")]
    Unauthorized(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("rate limited (HTTP 429): {0}")]
    RateLimited(String),

    #[error("unexpected response (HTTP {status}, content-type {content_type}): {body_preview}")]
    UnexpectedResponse {
        status: u16,
        content_type: String,
        body_preview: String,
    },

    #[error("API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
}
