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

impl ZendutyError {
    /// True when the upstream returned a 404 whose body is Zenduty's
    /// off-the-end paging sentinel (`{"detail":"Invalid page."}`).
    /// `list_incidents` translates this to an empty result; standard
    /// list semantics rather than a hard error.
    pub fn is_invalid_page_overshoot(&self) -> bool {
        matches!(self, ZendutyError::NotFound(body) if body.contains("Invalid page"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_page_overshoot_recognises_zenduty_sentinel() {
        let err = ZendutyError::NotFound(r#"{"detail":"Invalid page."}"#.into());
        assert!(err.is_invalid_page_overshoot());
    }

    #[test]
    fn invalid_page_overshoot_rejects_real_not_found() {
        let err = ZendutyError::NotFound(r#"{"detail":"Not found."}"#.into());
        assert!(!err.is_invalid_page_overshoot());
    }

    #[test]
    fn invalid_page_overshoot_rejects_non_not_found_errors() {
        let err = ZendutyError::Api {
            status: 500,
            message: "Invalid page".into(),
        };
        assert!(!err.is_invalid_page_overshoot());
    }
}
