pub mod audit;
pub mod error;

use std::sync::Arc;

use tracing::instrument;

pub use audit::GitProxyAuditLog;
pub use drua_git_proxy::{
    Allowlist, AllowlistConfig, AllowlistEntry, AllowlistError, GitProxyDecision, GitProxyMode,
    GitService, RefPatternSet, RepoCoord,
};
pub use error::GitProxyError;

use crate::auth::{error::AuthorizationError, AuthSubject};

/// Service surface for the git-proxy.
///
/// Holds the YAML-driven allow-list (in-memory; rebuilt at boot from
/// `[git_proxy.allowlist]` in `drua.yml`) and the audit-log writer.
/// `check_authorization` is the single per-request authorization
/// entry point — extracts `project_id` from `AuthSubject`, asserts
/// the subject is an Agent, then delegates ref/mode/repo enforcement
/// to lib's [`Allowlist`].
#[derive(Clone)]
pub struct GitProxies {
    allowlist: Arc<Allowlist>,
    audit: GitProxyAuditLog,
}

impl GitProxies {
    pub fn new(pool: &sqlx::PgPool, allowlist: Allowlist) -> Self {
        Self {
            allowlist: Arc::new(allowlist),
            audit: GitProxyAuditLog::new(pool),
        }
    }

    pub fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    pub fn audit(&self) -> &GitProxyAuditLog {
        &self.audit
    }

    /// Per-request authorization. Used by the axum handler in
    /// `server::git_proxy` and by integration tests.
    ///
    /// `refs_in_request` may be empty for `info/refs` GETs; for
    /// `git-receive-pack` POSTs the handler peeks the pkt-line stream
    /// and re-calls this with the parsed ref names.
    ///
    /// Fail-closed: subject without project_id, non-Agent subject,
    /// repo not in allow-list, mode missing, ref denied — all return
    /// `Err(_)` so the caller records the rejection and responds 403
    /// without ever spawning `git http-backend`.
    #[instrument(
        name = "domain.git_proxy.check_authorization",
        skip(self, sub, refs_in_request)
    )]
    pub fn check_authorization(
        &self,
        sub: &AuthSubject,
        owner: &str,
        repo: &str,
        mode: GitProxyMode,
        refs_in_request: &[String],
    ) -> Result<&AllowlistEntry, GitProxyError> {
        let project_id = sub
            .project_id()
            .ok_or(GitProxyError::SubjectMissingProject)?;
        if !sub.is_agent() {
            return Err(GitProxyError::Authorization(
                AuthorizationError::AuthenticationRequired,
            ));
        }

        let entry = self.allowlist.check_authorization(
            project_id.into(),
            owner,
            repo,
            mode,
            refs_in_request,
        )?;
        Ok(entry)
    }
}
