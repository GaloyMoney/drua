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
    /// Push to `ref_name` was denied because none of the configured
    /// `allowed_push_refs` globs match it. `allowed` is the list as
    /// configured in `drua.yml`; surfacing it in the rejection lets
    /// the agent self-correct without an out-of-band lookup.
    #[error(
        "AllowlistError - PushRefDenied: cannot push {ref_name} to {owner}/{repo} \
         (allowed: {allowed:?})"
    )]
    PushRefDenied {
        owner: String,
        repo: String,
        ref_name: String,
        allowed: Vec<String>,
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
            AllowlistError::PushRefDenied { .. } => "push_ref_denied",
        }
    }

    /// Multi-line human-readable detail rendered into the HTTP
    /// response body. Each line is prefixed with `remote:` by `git`
    /// when the client is on the receive-pack path, so a denied push
    /// surfaces all of this directly to the agent's stderr.
    pub fn detail(&self) -> String {
        match self {
            AllowlistError::PushRefDenied {
                owner,
                repo,
                ref_name,
                allowed,
            } => {
                let mut s = format!(
                    "push_ref_denied\n\
                         attempted: {ref_name}\n\
                         repo: {owner}/{repo}\n"
                );
                if allowed.is_empty() {
                    s.push_str("allowed: (none — this repo is configured pull-only or has no `allowed_push_refs`)\n");
                } else {
                    s.push_str("allowed:\n");
                    for p in allowed {
                        s.push_str(&format!("  - {p}\n"));
                    }
                }
                s
            }
            AllowlistError::RepoNotAllowed { owner, repo } => format!(
                "repo_not_allowed\n\
                 repo: {owner}/{repo} is not in the drua git-proxy allow-list\n"
            ),
            AllowlistError::ModeNotAllowed { owner, repo, mode } => format!(
                "mode_not_allowed\n\
                 repo: {owner}/{repo} does not permit `{mode}` operations\n"
            ),
            AllowlistError::InvalidRefPattern(e) => format!("invalid_ref_pattern: {e}\n"),
        }
    }
}
