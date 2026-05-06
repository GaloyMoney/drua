use thiserror::Error;

use crate::auth::error::AuthorizationError;

use super::repo::{
    GitProxyAllowlistEntryCreateError, GitProxyAllowlistEntryFindError,
    GitProxyAllowlistEntryModifyError, GitProxyAllowlistEntryQueryError,
};

#[derive(Error, Debug)]
pub enum GitProxyError {
    #[error("GitProxyError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("GitProxyError - Create: {0}")]
    Create(#[from] GitProxyAllowlistEntryCreateError),
    #[error("GitProxyError - Find: {0}")]
    Find(#[from] GitProxyAllowlistEntryFindError),
    #[error("GitProxyError - Modify: {0}")]
    Modify(#[from] GitProxyAllowlistEntryModifyError),
    #[error("GitProxyError - Query: {0}")]
    Query(#[from] GitProxyAllowlistEntryQueryError),
    #[error("GitProxyError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("GitProxyError - InvalidRepoCoord: owner={owner} repo={repo}")]
    InvalidRepoCoord { owner: String, repo: String },
    #[error("GitProxyError - RepoNotAllowed: project missing entry for {owner}/{repo}")]
    RepoNotAllowed { owner: String, repo: String },
    #[error("GitProxyError - ModeNotAllowed: {mode} not in allow-list modes for {owner}/{repo}")]
    ModeNotAllowed {
        owner: String,
        repo: String,
        mode: super::primitives::GitProxyMode,
    },
    #[error(
        "GitProxyError - RefPatternDenied: {ref_name} for {owner}/{repo} ({mode}) not in allowed patterns"
    )]
    RefPatternDenied {
        owner: String,
        repo: String,
        mode: super::primitives::GitProxyMode,
        ref_name: String,
    },
    #[error("GitProxyError - InvalidRefPattern: {0}")]
    InvalidRefPattern(#[from] globset::Error),
    #[error("GitProxyError - SubjectMissingProject")]
    SubjectMissingProject,
}

impl GitProxyError {
    /// Stable machine-readable code for the audit log + the response body.
    /// Agents see these strings in stderr; keep them short and grep-friendly.
    pub fn reject_code(&self) -> &'static str {
        match self {
            GitProxyError::InvalidRepoCoord { .. } => "invalid_repo_coord",
            GitProxyError::RepoNotAllowed { .. } => "repo_not_allowed",
            GitProxyError::ModeNotAllowed { .. } => "mode_not_allowed",
            GitProxyError::RefPatternDenied { .. } => "ref_pattern_denied",
            GitProxyError::Authorization(_) => "unauthorized",
            GitProxyError::SubjectMissingProject => "subject_missing_project",
            GitProxyError::InvalidRefPattern(_) => "invalid_ref_pattern",
            GitProxyError::Sqlx(_)
            | GitProxyError::Create(_)
            | GitProxyError::Find(_)
            | GitProxyError::Modify(_)
            | GitProxyError::Query(_) => "internal_error",
        }
    }
}
