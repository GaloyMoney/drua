use thiserror::Error;

use galoy_agents_core as domain;

use domain::mcp_creds::error::McpCredsError;
use domain::user::error::UserError;

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("AuthError - User: {0}")]
    User(#[from] UserError),
    #[error("AuthError - McpCreds: {0}")]
    McpCreds(#[from] McpCredsError),
    #[error("AuthError - OAuth: {0}")]
    OAuth(String),
    #[error("AuthError - CsrfMismatch")]
    CsrfMismatch,
    #[error("AuthError - TeamNotAuthorized")]
    TeamNotAuthorized,
    #[error("AuthError - Session: {0}")]
    Session(#[from] tower_sessions::session::Error),
    #[error("AuthError - GitHubApi: {0}")]
    GitHubApi(#[from] reqwest::Error),
    #[error("AuthError - InvalidToken")]
    InvalidToken,
}

impl axum::response::IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!(%self, "auth error");
        match &self {
            AuthError::TeamNotAuthorized => axum::response::Redirect::to(
                "/?error=Your+GitHub+account+is+not+in+an+authorized+team",
            )
            .into_response(),
            AuthError::CsrfMismatch => {
                (axum::http::StatusCode::FORBIDDEN, self.to_string()).into_response()
            }
            _ => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                self.to_string(),
            )
                .into_response(),
        }
    }
}
