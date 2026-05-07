use thiserror::Error;

use crate::auth::error::AuthorizationError;

pub use drua_git_proxy::AllowlistError;

/// Core-side wrapper around lib's [`AllowlistError`] that layers in
/// drua's auth concerns (subject must be Agent, etc.). HTTP handlers
/// in `server::git_proxy` map variants to status codes via
/// [`Self::reject_code`].
#[derive(Error, Debug)]
pub enum GitProxyError {
    #[error("GitProxyError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("GitProxyError - Allowlist: {0}")]
    Allowlist(#[from] AllowlistError),
    #[error("GitProxyError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("GitProxyError - InvalidRepoCoord: owner={owner} repo={repo}")]
    InvalidRepoCoord { owner: String, repo: String },
    #[error("GitProxyError - SubjectMissingProject")]
    SubjectMissingProject,
}

impl GitProxyError {
    /// Stable machine-readable code for the audit log + the HTTP
    /// response body. Agents see these strings in stderr.
    pub fn reject_code(&self) -> &'static str {
        match self {
            GitProxyError::InvalidRepoCoord { .. } => "invalid_repo_coord",
            GitProxyError::Authorization(_) => "unauthorized",
            GitProxyError::SubjectMissingProject => "subject_missing_project",
            GitProxyError::Allowlist(e) => e.reject_code(),
            GitProxyError::Sqlx(_) => "internal_error",
        }
    }
}
