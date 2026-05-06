use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::stream::StreamDelta;
use crate::{Prompt, PromptResponse};

#[derive(Debug)]
pub struct PromptRequest {
    pub prompt: Prompt,
    pub response_channel: PromptResponseChannel,
}

impl PromptRequest {
    /// Returns the request to dispatch and the response receiver to await.
    pub fn new(prompt: Prompt) -> (Self, oneshot::Receiver<Result<PromptResult, PromptError>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                prompt,
                response_channel: tx,
            },
            rx,
        )
    }
}

pub type PromptRequestChannel = mpsc::Sender<PromptRequest>;
pub type PromptResponseChannel = oneshot::Sender<Result<PromptResult, PromptError>>;

pub struct StreamHandle {
    pub rx: mpsc::Receiver<Result<StreamDelta, PromptError>>,
}

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum PromptResult {
    Complete(PromptResponse),
    Stream(StreamHandle),
}

/// `Transient` triggers chain fallback; `Terminal` propagates immediately.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PromptError {
    #[error("transient {kind:?}: {message}")]
    Transient {
        kind: TransientKind,
        message: String,
        retry_after: Option<Duration>,
    },

    #[error("terminal {kind:?}: {message}")]
    Terminal {
        kind: TerminalKind,
        message: String,
    },

    #[error("model `{0}` not configured")]
    ModelNotConfigured(String),

    /// Unclassified — treated as transient during migration.
    #[error("provider: {0}")]
    Provider(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientKind {
    Connection,
    Timeout,
    RateLimit,
    ServerError,
    EmptyCompletion,
    SseDecode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    Auth,
    BadRequest,
    ContextWindow,
    ContentPolicy,
    NotFound,
}

impl PromptError {
    /// Returns true when the chain walker should NOT advance to the
    /// next entry. `Provider(_)` falls back (safe default).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            PromptError::Terminal { .. } | PromptError::ModelNotConfigured(_)
        )
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            PromptError::Transient { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl PromptError {
    pub fn transient(kind: TransientKind, message: impl Into<String>) -> Self {
        Self::Transient {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn transient_with_retry(
        kind: TransientKind,
        message: impl Into<String>,
        retry_after: Duration,
    ) -> Self {
        Self::Transient {
            kind,
            message: message.into(),
            retry_after: Some(retry_after),
        }
    }

    pub fn terminal(kind: TerminalKind, message: impl Into<String>) -> Self {
        Self::Terminal {
            kind,
            message: message.into(),
        }
    }

    /// Classify an upstream HTTP status into the right `PromptError` arm.
    pub fn from_http_status(status: u16, body: impl Into<String>) -> Self {
        match status {
            400 => Self::terminal(TerminalKind::BadRequest, body),
            401 | 403 => Self::terminal(TerminalKind::Auth, body),
            404 => Self::terminal(TerminalKind::NotFound, body),
            408 => Self::transient(TransientKind::Timeout, body),
            413 => Self::terminal(TerminalKind::ContextWindow, body),
            422 => Self::terminal(TerminalKind::ContentPolicy, body),
            429 => Self::transient(TransientKind::RateLimit, body),
            500..=599 => Self::transient(TransientKind::ServerError, body),
            _ => Self::Provider(format!("status={status}: {}", body.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_http_status_examples() {
        assert!(PromptError::from_http_status(401, "x").is_terminal());
        assert!(PromptError::from_http_status(403, "x").is_terminal());
        assert!(PromptError::from_http_status(400, "x").is_terminal());
        assert!(PromptError::from_http_status(413, "x").is_terminal());
        assert!(PromptError::from_http_status(422, "x").is_terminal());

        assert!(!PromptError::from_http_status(429, "x").is_terminal());
        assert!(!PromptError::from_http_status(500, "x").is_terminal());
        assert!(!PromptError::from_http_status(503, "x").is_terminal());
        assert!(!PromptError::from_http_status(408, "x").is_terminal());
    }

    #[test]
    fn unclassified_provider_falls_back() {
        assert!(!PromptError::Provider("legacy".into()).is_terminal());
    }
}
