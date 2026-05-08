pub mod error;

use std::sync::Arc;

use async_trait::async_trait;
use tracing::instrument;

pub use drua_git_proxy::{
    parse_command_list, push_to_upstream, spawn_http_backend, Allowlist, AllowlistConfig,
    AllowlistEntry, AllowlistError, CgiError, CgiRequest, CgiResponse, GitProxyMode, GitService,
    MirrorConfig, MirrorError, MirrorManager, PktLineError, RefPatternSet, RefUpdate, RepoCoord,
    StaticCredential, UpstreamCredentialProvider,
};
pub use error::GitProxyError;

use crate::auth::{error::AuthorizationError, AuthSubject};
use crate::github_app::GitHubAppTokenProvider;

/// Bridge `GitHubAppTokenProvider` to lib's
/// [`UpstreamCredentialProvider`] so the leaf crate stays
/// drua-core-free. Mints a fresh installation token per call.
struct AppTokenCreds(Arc<GitHubAppTokenProvider>);

#[async_trait]
impl UpstreamCredentialProvider for AppTokenCreds {
    async fn fresh_token(&self) -> Result<String, MirrorError> {
        self.0
            .generate_token()
            .await
            .map(|t| t.token)
            .map_err(|e| MirrorError::CredentialProvider(e.to_string()))
    }
}

/// Service surface for the git-proxy. Holds the YAML-driven
/// allow-list (rebuilt at boot from `[git_proxy.allowlist]` in
/// `drua.yml`), the bare-mirror manager, and a credential bridge to
/// the existing `GitHubAppTokenProvider`.
///
/// Per-request decisions land in `audit_entries` via the global
/// `Audit` service (see `core::audit::Audit::record_*`); this
/// service holds no audit state of its own.
#[derive(Clone)]
pub struct GitProxies {
    allowlist: Arc<Allowlist>,
    mirror: Option<MirrorManager>,
    creds: Option<Arc<dyn UpstreamCredentialProvider>>,
}

impl GitProxies {
    pub fn new(allowlist: Allowlist) -> Self {
        Self {
            allowlist: Arc::new(allowlist),
            mirror: None,
            creds: None,
        }
    }

    /// Wires in the bare mirror manager + the GitHub App credential
    /// bridge. `None` for either disables the upstream-fetch path
    /// (handler returns 503 with a stable code) which is what we want
    /// when the proxy is being run without a configured GitHub App
    /// (local dev with no upstream).
    pub fn with_mirror(
        mut self,
        mirror: MirrorManager,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Self {
        self.mirror = Some(mirror);
        self.creds =
            github_app.map(|a| Arc::new(AppTokenCreds(a)) as Arc<dyn UpstreamCredentialProvider>);
        self
    }

    /// Test-only entry point — accepts any [`UpstreamCredentialProvider`].
    pub fn with_credentials(mut self, creds: Arc<dyn UpstreamCredentialProvider>) -> Self {
        self.creds = Some(creds);
        self
    }

    pub fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    pub fn mirror(&self) -> Option<&MirrorManager> {
        self.mirror.as_ref()
    }

    pub fn credentials(&self) -> Option<&dyn UpstreamCredentialProvider> {
        self.creds.as_deref()
    }

    /// Per-request authorization. Used by the axum handler in
    /// `server::git_proxy` and by integration tests.
    ///
    /// `refs_in_request` may be empty for `info/refs` GETs; for
    /// `git-receive-pack` POSTs the handler peeks the pkt-line stream
    /// and re-calls this with the parsed ref names.
    ///
    /// **Authorization model**: deliberately bypasses
    /// `sub.can(AuthVerb, AuthResource)` — the YAML allow-list IS
    /// the policy (memo `019dfebc` §7.2). The gate is "is this an
    /// Agent" + "does the global allow-list permit `(owner, repo,
    /// mode, refs)`". `sub.project_id()` is recorded in the audit
    /// row for attribution but doesn't gate the decision.
    ///
    /// Fail-closed: non-Agent subject, repo not in allow-list, mode
    /// missing, ref denied — all return `Err(_)` so the caller records
    /// the rejection and responds 403 without ever spawning
    /// `git http-backend`.
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
        if !sub.is_agent() {
            return Err(GitProxyError::Authorization(
                AuthorizationError::AuthenticationRequired,
            ));
        }
        let entry = self
            .allowlist
            .check_authorization(owner, repo, mode, refs_in_request)?;
        Ok(entry)
    }
}
