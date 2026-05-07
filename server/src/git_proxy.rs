//! Smart-HTTP git proxy mounted at `/git/{owner}/{repo}/...`.
//!
//! Per memo `019dfebc`: authenticate the sandbox via the shared
//! `AuthSubject` extension (already populated by the global
//! `auth_middleware` from a projected SA token or, in dev, a
//! `dev-agent:<uuid>` token), authorize the
//! `(project_id, owner, repo, mode)` tuple against the per-project
//! YAML allow-list, audit-log the decision, then forward to upstream
//! `git http-backend` against the per-project bare mirror.
//!
//! The handler never touches a GitHub credential — the credential
//! provider on `GitProxies` mints a fresh installation token per
//! upstream fetch via `GitHubAppTokenProvider`. The sandbox holds
//! only its SA token; the App token never crosses the trust boundary.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Router,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde::Deserialize;
use tracing::instrument;

use drua_core::auth::AuthSubject;
use drua_core::git_proxy::{
    spawn_http_backend, CgiRequest, GitProxyDecision, GitProxyError, GitService, RepoCoord,
};

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
    headers: HeaderMap,
) -> Response {
    tracing::Span::current().record("owner", owner.as_str());
    tracing::Span::current().record("repo", repo.as_str());

    let Some(service_str) = q.service.as_deref() else {
        return reject_response(StatusCode::BAD_REQUEST, "missing_service_query");
    };
    tracing::Span::current().record("service", service_str);

    let Some(service) = GitService::from_query(service_str) else {
        return reject_response(StatusCode::BAD_REQUEST, "invalid_service");
    };

    let query_string = format!("service={service_str}");
    handle(
        &auth,
        &state,
        &owner,
        &repo,
        service,
        &[],
        Forward {
            method: "GET",
            path_info: "/info/refs",
            query_string: &query_string,
            headers: &headers,
            body: Bytes::new(),
        },
    )
    .await
}

#[instrument(name = "web.git_proxy.git_upload_pack", skip_all, fields(owner, repo))]
async fn git_upload_pack(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    tracing::Span::current().record("owner", owner.as_str());
    tracing::Span::current().record("repo", repo.as_str());
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to buffer upload-pack body");
            return reject_response(StatusCode::BAD_REQUEST, "body_read_error");
        }
    };
    handle(
        &auth,
        &state,
        &owner,
        &repo,
        GitService::GitUploadPack,
        &[],
        Forward {
            method: "POST",
            path_info: "/git-upload-pack",
            query_string: "",
            headers: &headers,
            body: body_bytes,
        },
    )
    .await
}

#[instrument(name = "web.git_proxy.git_receive_pack", skip_all, fields(owner, repo))]
async fn git_receive_pack(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    tracing::Span::current().record("owner", owner.as_str());
    tracing::Span::current().record("repo", repo.as_str());
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to buffer receive-pack body");
            return reject_response(StatusCode::BAD_REQUEST, "body_read_error");
        }
    };

    let coord = match RepoCoord::parse(&owner, &repo) {
        Some(c) => c,
        None => return reject_response(StatusCode::BAD_REQUEST, "invalid_repo_coord"),
    };
    if !auth.is_agent() {
        let _ = audit_reject(
            &state,
            &auth,
            &coord,
            GitService::GitReceivePack,
            "unauthorized",
        )
        .await;
        return reject_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    // Peek the pkt-line cmd list BEFORE spawning git-http-backend so we
    // can deny non-allowed refs before any object touches disk.
    let updates = match drua_core::git_proxy::parse_command_list(&body_bytes) {
        Ok(refs) => refs,
        Err(e) => {
            tracing::warn!(error = %e, "git-proxy: failed to peek receive-pack pkt-line");
            let _ = audit_reject(
                &state,
                &auth,
                &coord,
                GitService::GitReceivePack,
                "malformed_receive_pack",
            )
            .await;
            return reject_response(StatusCode::BAD_REQUEST, "malformed_receive_pack");
        }
    };
    let ref_names: Vec<String> = updates.iter().map(|u| u.ref_name.clone()).collect();

    let upstream_url = match state.app.git_proxies().check_authorization(
        &auth,
        &coord.owner,
        &coord.repo,
        GitService::GitReceivePack.mode(),
        &ref_names,
    ) {
        Ok(entry) => entry.upstream_url.clone(),
        Err(err) => {
            return render_authz_error(&state, &auth, &coord, GitService::GitReceivePack, err).await
        }
    };

    let attempt_id = match state
        .app
        .git_proxies()
        .audit()
        .record_attempt(
            auth.acting_agent_id(),
            auth.project_id(),
            &coord.owner,
            &coord.repo,
            GitService::GitReceivePack,
            serde_json::to_value(&updates).unwrap_or(serde_json::Value::Null),
            drua_core::git_proxy::GitProxyDecision::Accepted,
            None,
        )
        .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!(error = %e, "audit accept insert failed");
            None
        }
    };

    let cgi_response = forward_to_backend(
        &state,
        &auth,
        &coord,
        &upstream_url,
        GitService::GitReceivePack,
        Forward {
            method: "POST",
            path_info: "/git-receive-pack",
            query_string: "",
            headers: &headers,
            body: body_bytes,
        },
        attempt_id,
    )
    .await;

    // Mirror the accepted refs upstream. If the push fails the proxy
    // logs but still returns the local receive-pack success body —
    // the audit row records the failure so ops can chase it.
    let project_id = auth.project_id().expect("is_agent checked earlier");
    let mirror_path = state
        .app
        .git_proxies()
        .mirror()
        .expect("mirror configured")
        .mirror_path(project_id.into(), &coord);
    let static_creds = drua_core::git_proxy::StaticCredential(String::new());
    let creds: &dyn drua_core::git_proxy::UpstreamCredentialProvider =
        if upstream_url.starts_with("file://") {
            &static_creds
        } else {
            state
                .app
                .git_proxies()
                .credentials()
                .unwrap_or(&static_creds)
        };
    if let Err(e) =
        drua_core::git_proxy::push_to_upstream(&mirror_path, &upstream_url, &updates, creds).await
    {
        tracing::warn!(error = %e, "git-proxy: upstream forward failed");
    }

    cgi_response
}

/// Forwarding inputs the handler needs for a CGI-spawn dispatch.
struct Forward<'a> {
    method: &'a str,
    path_info: &'a str,
    query_string: &'a str,
    headers: &'a HeaderMap,
    body: Bytes,
}

/// Single dispatch: parse → authorize → audit → forward (pull-side
/// only — receive-pack is handled inline above).
async fn handle(
    auth: &AuthSubject,
    state: &AppState,
    owner_raw: &str,
    repo_raw: &str,
    service: GitService,
    refs_in_request: &[String],
    fwd: Forward<'_>,
) -> Response {
    let mode = service.mode();

    let coord = match RepoCoord::parse(owner_raw, repo_raw) {
        Some(c) => c,
        None => return reject_response(StatusCode::BAD_REQUEST, "invalid_repo_coord"),
    };

    if !auth.is_agent() {
        let _ = audit_reject(state, auth, &coord, service, "unauthorized").await;
        return reject_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    let upstream_url = match state.app.git_proxies().check_authorization(
        auth,
        &coord.owner,
        &coord.repo,
        mode,
        refs_in_request,
    ) {
        Ok(entry) => entry.upstream_url.clone(),
        Err(err) => return render_authz_error(state, auth, &coord, service, err).await,
    };

    let attempt_id = match state
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
    {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!(error = %e, "audit accept insert failed");
            None
        }
    };

    forward_to_backend(state, auth, &coord, &upstream_url, service, fwd, attempt_id).await
}

async fn forward_to_backend(
    state: &AppState,
    auth: &AuthSubject,
    coord: &RepoCoord,
    upstream_url: &str,
    service: GitService,
    fwd: Forward<'_>,
    attempt_id: Option<uuid::Uuid>,
) -> Response {
    let project_id = match auth.project_id() {
        Some(p) => p,
        None => return reject_response(StatusCode::UNAUTHORIZED, "subject_missing_project"),
    };

    let proxies = state.app.git_proxies();
    let Some(mirror) = proxies.mirror() else {
        tracing::warn!("git-proxy: mirror manager not configured");
        return reject_response(StatusCode::SERVICE_UNAVAILABLE, "mirror_disabled");
    };
    // file:// upstreams (test fixtures) don't need credentials; production
    // https:// upstreams must have a credential provider configured.
    let static_creds: drua_core::git_proxy::StaticCredential =
        drua_core::git_proxy::StaticCredential(String::new());
    let creds: &dyn drua_core::git_proxy::UpstreamCredentialProvider =
        if upstream_url.starts_with("file://") {
            &static_creds
        } else {
            match proxies.credentials() {
                Some(c) => c,
                None => {
                    tracing::warn!("git-proxy: upstream credentials not configured");
                    return reject_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "upstream_credentials_missing",
                    );
                }
            }
        };

    let mirror_path = match mirror
        .ensure(project_id.into(), coord, upstream_url, creds)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "git-proxy: mirror ensure failed");
            return reject_response(StatusCode::BAD_GATEWAY, "mirror_unavailable");
        }
    };

    let content_type = fwd
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let content_encoding = fwd
        .headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok());

    let req_in = CgiRequest {
        method: fwd.method,
        path_info: fwd.path_info,
        query_string: fwd.query_string,
        content_type,
        content_encoding,
        body: fwd.body,
    };
    let bytes_received = req_in.body.len() as i64;

    let cgi_resp = match spawn_http_backend(&mirror_path, &req_in).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "git-proxy: http-backend spawn failed");
            return reject_response(StatusCode::BAD_GATEWAY, "backend_spawn_failed");
        }
    };

    if let Some(id) = attempt_id {
        let bytes_sent = cgi_resp.body.len() as i64;
        let upstream_status = Some(cgi_resp.status.as_u16() as i32);
        let _ = proxies
            .audit()
            .record_completion(id, bytes_sent, bytes_received, upstream_status)
            .await
            .map_err(|e| tracing::error!(error = %e, "audit completion update failed"));
    }
    let _ = service;

    let mut response = Response::builder().status(cgi_resp.status);
    if let Some(headers_mut) = response.headers_mut() {
        for (name, value) in cgi_resp.headers.iter() {
            headers_mut.append(name, value.clone());
        }
    }
    response
        .body(Body::from(cgi_resp.body))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to build response from CGI body");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal_error").into_response()
        })
}

async fn render_authz_error(
    state: &AppState,
    auth: &AuthSubject,
    coord: &RepoCoord,
    service: GitService,
    err: GitProxyError,
) -> Response {
    let code = err.reject_code();
    tracing::warn!(
        error = %err,
        owner = %coord.owner,
        repo = %coord.repo,
        service = service.as_str(),
        code = code,
        "git-proxy rejected request"
    );
    let _ = audit_reject(state, auth, coord, service, code).await;
    let status = match err {
        GitProxyError::Authorization(_) | GitProxyError::SubjectMissingProject => {
            StatusCode::UNAUTHORIZED
        }
        GitProxyError::Allowlist(drua_core::git_proxy::AllowlistError::RepoNotAllowed {
            ..
        })
        | GitProxyError::Allowlist(drua_core::git_proxy::AllowlistError::ModeNotAllowed {
            ..
        })
        | GitProxyError::Allowlist(drua_core::git_proxy::AllowlistError::RefPatternDenied {
            ..
        }) => StatusCode::FORBIDDEN,
        GitProxyError::InvalidRepoCoord { .. }
        | GitProxyError::Allowlist(drua_core::git_proxy::AllowlistError::InvalidRefPattern(_)) => {
            StatusCode::BAD_REQUEST
        }
        GitProxyError::Sqlx(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    reject_response(status, code)
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
