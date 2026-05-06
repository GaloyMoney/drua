//! Smart-HTTP git proxy mounted at `/git/:owner/:repo.git/...`.
//!
//! Per memo `019dfebc` (M1 milestone): authenticate the sandbox via the
//! shared `AuthSubject` extension (already populated by the global
//! `auth_middleware` from a projected SA token), authorize the
//! `(project_id, owner, repo, mode)` tuple against the per-project
//! allow-list, audit-log the decision, then either forward to upstream
//! `git http-backend` (M1.5) or return 501.
//!
//! This handler never touches a GitHub credential — credential minting
//! lives behind `GitHubAppTokenProvider` on the upstream-forward path
//! (M1.5). The sandbox holds only the SA token; the App token never
//! crosses the trust boundary.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Router,
};
use serde::Deserialize;
use tracing::instrument;

use drua_core::auth::AuthSubject;
use drua_core::git_proxy::{GitProxyDecision, GitProxyError, GitService, RepoCoord};

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/git/{owner}/{repo}/info/refs", get(info_refs))
        .route("/git/{owner}/{repo}/git-upload-pack", post(git_upload_pack))
        .route(
            "/git/{owner}/{repo}/git-receive-pack",
            post(git_receive_pack),
        )
}

#[derive(Debug, Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

#[instrument(
    name = "web.git_proxy.info_refs",
    skip_all,
    fields(owner, repo, service)
)]
async fn info_refs(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<InfoRefsQuery>,
) -> Response {
    tracing::Span::current().record("owner", owner.as_str());
    tracing::Span::current().record("repo", repo.as_str());

    let Some(service_str) = q.service.as_deref() else {
        // Stock `info/refs` (no service= query) is the dumb-HTTP path.
        // We don't support it — every modern client sends service=...
        return reject_response(StatusCode::BAD_REQUEST, "missing_service_query");
    };
    tracing::Span::current().record("service", service_str);

    let Some(service) = GitService::from_query(service_str) else {
        return reject_response(StatusCode::BAD_REQUEST, "invalid_service");
    };

    handle(&auth, &state, &owner, &repo, service, &[]).await
}

#[instrument(name = "web.git_proxy.git_upload_pack", skip_all, fields(owner, repo))]
async fn git_upload_pack(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Response {
    tracing::Span::current().record("owner", owner.as_str());
    tracing::Span::current().record("repo", repo.as_str());
    handle(&auth, &state, &owner, &repo, GitService::GitUploadPack, &[]).await
}

#[instrument(name = "web.git_proxy.git_receive_pack", skip_all, fields(owner, repo))]
async fn git_receive_pack(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Response {
    tracing::Span::current().record("owner", owner.as_str());
    tracing::Span::current().record("repo", repo.as_str());
    // M1.5: parse pkt-line cmds from body to populate refs_in_request
    // and re-check policy per-ref. For M1 we authorize on mode only;
    // the per-ref enforcement lives in the pre-receive hook (deferred
    // along with `git http-backend` spawn).
    handle(
        &auth,
        &state,
        &owner,
        &repo,
        GitService::GitReceivePack,
        &[],
    )
    .await
}

/// Single dispatch: parse → authorize → audit → forward (or 501).
async fn handle(
    auth: &AuthSubject,
    state: &AppState,
    owner_raw: &str,
    repo_raw: &str,
    service: GitService,
    refs_in_request: &[String],
) -> Response {
    let mode = service.mode();

    let coord = match RepoCoord::parse(owner_raw, repo_raw) {
        Some(c) => c,
        None => {
            // Don't even audit-log this — it's a malformed URL, not a
            // policy decision. The empty owner/repo would violate the
            // table NOT NULL constraints anyway.
            return reject_response(StatusCode::BAD_REQUEST, "invalid_repo_coord");
        }
    };

    if !auth.is_agent() {
        let _ = audit_reject(state, auth, &coord, service, "unauthorized").await;
        return reject_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    match state
        .app
        .git_proxies()
        .check_authorization(auth, &coord.owner, &coord.repo, mode, refs_in_request)
        .await
    {
        Ok(_entry) => {
            let _ = state
                .app
                .git_proxies()
                .audit()
                .record_attempt(
                    auth.acting_agent_id(),
                    auth.project_id(),
                    &coord.owner,
                    &coord.repo,
                    service,
                    serde_json::Value::Array(
                        refs_in_request
                            .iter()
                            .map(|r| serde_json::Value::String(r.clone()))
                            .collect(),
                    ),
                    GitProxyDecision::Accepted,
                    None,
                )
                .await
                .map_err(|e| tracing::error!(error = %e, "audit insert failed"));

            // M1.5: spawn `git http-backend` against the per-project
            // mirror, install the pre-receive hook, forward upstream
            // via `GitHubAppTokenProvider::generate_token()`. For M1
            // the policy + audit + URL contract are settled and we
            // explicitly stage a 501 so the bats e2e fails with a
            // recognisable shape that the next commit replaces.
            (
                StatusCode::NOT_IMPLEMENTED,
                [(axum::http::header::CONTENT_TYPE, "text/plain")],
                "git-proxy: policy accepted; backend forwarding lands in M1.5\n",
            )
                .into_response()
        }
        Err(err) => {
            let code = err.reject_code();
            tracing::warn!(
                error = %err,
                owner = %coord.owner,
                repo = %coord.repo,
                service = service.as_str(),
                code = code,
                "git-proxy rejected request"
            );
            let _ = audit_reject(state, auth, &coord, service, code).await;
            let status = match err {
                GitProxyError::Authorization(_) | GitProxyError::SubjectMissingProject => {
                    StatusCode::UNAUTHORIZED
                }
                GitProxyError::RepoNotAllowed { .. }
                | GitProxyError::ModeNotAllowed { .. }
                | GitProxyError::RefPatternDenied { .. } => StatusCode::FORBIDDEN,
                GitProxyError::InvalidRepoCoord { .. } | GitProxyError::InvalidRefPattern(_) => {
                    StatusCode::BAD_REQUEST
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            reject_response(status, code)
        }
    }
}

async fn audit_reject(
    state: &AppState,
    auth: &AuthSubject,
    coord: &RepoCoord,
    service: GitService,
    reason: &str,
) -> Result<uuid::Uuid, sqlx::Error> {
    state
        .app
        .git_proxies()
        .audit()
        .record_attempt(
            auth.acting_agent_id(),
            auth.project_id(),
            &coord.owner,
            &coord.repo,
            service,
            serde_json::Value::Array(vec![]),
            GitProxyDecision::Rejected,
            Some(reason),
        )
        .await
}

fn reject_response(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        format!("git-proxy: {code}\n"),
    )
        .into_response()
}
