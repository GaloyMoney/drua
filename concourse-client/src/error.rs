use thiserror::Error;

/// Errors returned by the Concourse client.
#[derive(Debug, Error)]
pub enum ConcourseError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("failed to parse URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
}
