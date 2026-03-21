use thiserror::Error;

use crate::agent::error::AgentError;
use crate::user::error::UserError;

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("AuthError - User: {0}")]
    User(#[from] UserError),
    #[error("AuthError - Agent: {0}")]
    Agent(#[from] AgentError),
    #[error("AuthError - OAuth: {0}")]
    OAuth(String),
    #[error("AuthError - CsrfMismatch")]
    CsrfMismatch,
    #[error("AuthError - Session: {0}")]
    Session(#[from] tower_sessions::session::Error),
    #[error("AuthError - GitHubApi: {0}")]
    GitHubApi(#[from] reqwest::Error),
}

impl axum::response::IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!(error = %self, "auth error");
        let status = match &self {
            AuthError::CsrfMismatch => axum::http::StatusCode::FORBIDDEN,
            AuthError::OAuth(_) | AuthError::GitHubApi(_) => axum::http::StatusCode::BAD_GATEWAY,
            _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = match status {
            axum::http::StatusCode::FORBIDDEN => "Forbidden",
            axum::http::StatusCode::BAD_GATEWAY => "Authentication provider error",
            _ => "Internal server error",
        };
        (status, body).into_response()
    }
}
