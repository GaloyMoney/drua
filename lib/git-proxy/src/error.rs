use thiserror::Error;

use crate::primitives::GitProxyMode;

/// Errors raised by the YAML-driven allow-list during config-load
/// and per-request authorization. Wrapped by core's `GitProxyError`
/// so HTTP handlers can layer in core's authorization concerns.
#[derive(Error, Debug)]
pub enum AllowlistError {
    #[error("AllowlistError - InvalidRefPattern: {0}")]
    InvalidRefPattern(#[from] globset::Error),
    #[error("AllowlistError - RepoNotAllowed: project missing entry for {owner}/{repo}")]
    RepoNotAllowed { owner: String, repo: String },
    #[error("AllowlistError - ModeNotAllowed: {mode} not in allow-list modes for {owner}/{repo}")]
    ModeNotAllowed {
        owner: String,
        repo: String,
        mode: GitProxyMode,
    },
    #[error(
        "AllowlistError - RefPatternDenied: {ref_name} for {owner}/{repo} ({mode}) not in allowed patterns"
    )]
    RefPatternDenied {
        owner: String,
        repo: String,
        mode: GitProxyMode,
        ref_name: String,
    },
}

impl AllowlistError {
    /// Stable machine-readable code for the audit log + the HTTP response
    /// body. Agents see these strings in stderr; keep them grep-friendly.
    pub fn reject_code(&self) -> &'static str {
        match self {
            AllowlistError::InvalidRefPattern(_) => "invalid_ref_pattern",
            AllowlistError::RepoNotAllowed { .. } => "repo_not_allowed",
            AllowlistError::ModeNotAllowed { .. } => "mode_not_allowed",
            AllowlistError::RefPatternDenied { .. } => "ref_pattern_denied",
        }
    }
}
