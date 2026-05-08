//! Smart-HTTP git proxy mounted at `/git/{owner}/{repo}/...`.
//!
//! Per memo `019dfebc`: authenticate the sandbox via the shared
//! `AuthSubject` extension (already populated by the global
//! `auth_middleware` from a projected SA token, or in dev from
//! the literal `dev-agent` Bearer token), authorize the
//! `(owner, repo, mode, refs)` tuple against the global YAML
//! allow-list, audit-log the decision (with project_id for
//! attribution), then forward to upstream `git http-backend`
//! against the bare mirror.
//!
//! The handler never touches a GitHub credential — the credential
//! provider on `GitProxies` mints a fresh installation token per
//! upstream fetch via `GitHubAppTokenProvider`. The sandbox holds
//! only its SA token; the App token never crosses the trust boundary.
//!
//! Audit: every request lands in `audit_entries` via the shared
//! `core::audit::Audit::record_*` API. The handler enriches the
//! request's audit context with proxy-specific fields and the global
//! `audit_middleware` persists the row at request boundary
//! (the GET-skip rule is suppressed for `/git/` so `info/refs`
//! advertisements are recorded too). Wire shape:
//! `action = "git_proxy.{service}"`, outcome `Success` for accepts,
//! `Error { message: <reject_code> }` for rejects;
//! `metadata = {owner, repo, refs}`. Bytes / upstream HTTP status are
//! NOT in the audit row — they're tracing fields on the handler span
//! so Honeycomb can query them without the audit table bloating.

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

use drua_core::audit::{primitives::InteractionOutcome, Audit};
use drua_core::auth::AuthSubject;
use drua_core::git_proxy::{spawn_http_backend, CgiRequest, GitProxyError, GitService, RepoCoord};

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
    fields(owner, repo, service, bytes_sent, upstream_status)
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
        return reject_response(&auth, StatusCode::BAD_REQUEST, "missing_service_query");
    };
    tracing::Span::current().record("service", service_str);

    let Some(service) = GitService::from_query(service_str) else {
        return reject_response(&auth, StatusCode::BAD_REQUEST, "invalid_service");
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

#[instrument(
    name = "web.git_proxy.git_upload_pack",
    skip_all,
    fields(owner, repo, bytes_sent, bytes_received, upstream_status)
)]
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
            return reject_response(&auth, StatusCode::BAD_REQUEST, "body_read_error");
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

#[instrument(
    name = "web.git_proxy.git_receive_pack",
    skip_all,
    fields(owner, repo, bytes_sent, bytes_received, upstream_status)
)]
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
            return reject_response(&auth, StatusCode::BAD_REQUEST, "body_read_error");
        }
    };

    let coord = match RepoCoord::parse(&owner, &repo) {
        Some(c) => c,
        None => return reject_response(&auth, StatusCode::BAD_REQUEST, "invalid_repo_coord"),
    };

    // Peek the pkt-line cmd list BEFORE spawning git-http-backend so we
    // can deny non-allowed refs before any object touches disk.
    let updates = match drua_core::git_proxy::parse_command_list(&body_bytes) {
        Ok(refs) => refs,
        Err(e) => {
            tracing::warn!(error = %e, "git-proxy: failed to peek receive-pack pkt-line");
            seed_audit_context(&auth, &coord, GitService::GitReceivePack, &[]);
            return finish_audit_reject(StatusCode::BAD_REQUEST, "malformed_receive_pack");
        }
    };
    let ref_names: Vec<String> = updates.iter().map(|u| u.ref_name.clone()).collect();

    seed_audit_context(&auth, &coord, GitService::GitReceivePack, &ref_names);

    let upstream_url = match state.app.git_proxies().check_authorization(
        &auth,
        &coord.owner,
        &coord.repo,
        GitService::GitReceivePack.mode(),
        &ref_names,
    ) {
        Ok(entry) => entry.upstream_url.clone(),
        Err(err) => return finish_authz_reject(err),
    };

    let cgi_response = forward_to_backend(
        &state,
        &coord,
        &upstream_url,
        Forward {
            method: "POST",
            path_info: "/git-receive-pack",
            query_string: "",
            headers: &headers,
            body: body_bytes,
        },
    )
    .await;

    // Mirror the accepted refs upstream. If the push fails the proxy
    // logs but still returns the local receive-pack success body —
    // ops sees the upstream failure via Honeycomb (push_to_upstream's
    // own span) and the audit row's outcome.
    let mirror_path = state
        .app
        .git_proxies()
        .mirror()
        .expect("mirror configured")
        .mirror_path(&coord);
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

    finish_audit(&cgi_response);
    cgi_response
}

struct Forward<'a> {
    method: &'a str,
    path_info: &'a str,
    query_string: &'a str,
    headers: &'a HeaderMap,
    body: Bytes,
}

/// Pull-side dispatch: parse → authorize (via service, fail-closed
/// for non-Agent subjects) → audit → spawn `git http-backend`. The
/// receive-pack handler is inline above because it needs an extra
/// pkt-line peek step before authz.
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
        None => return reject_response(auth, StatusCode::BAD_REQUEST, "invalid_repo_coord"),
    };

    seed_audit_context(auth, &coord, service, refs_in_request);

    let upstream_url = match state.app.git_proxies().check_authorization(
        auth,
        &coord.owner,
        &coord.repo,
        mode,
        refs_in_request,
    ) {
        Ok(entry) => entry.upstream_url.clone(),
        Err(err) => return finish_authz_reject(err),
    };

    let response = forward_to_backend(state, &coord, &upstream_url, fwd).await;
    finish_audit(&response);
    response
}

async fn forward_to_backend(
    state: &AppState,
    coord: &RepoCoord,
    upstream_url: &str,
    fwd: Forward<'_>,
) -> Response {
    let proxies = state.app.git_proxies();
    let Some(mirror) = proxies.mirror() else {
        tracing::warn!("git-proxy: mirror manager not configured");
        return reject_status(StatusCode::SERVICE_UNAVAILABLE, "mirror_disabled");
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
                    return reject_status(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "upstream_credentials_missing",
                    );
                }
            }
        };

    let mirror_path = match mirror.ensure(coord, upstream_url, creds).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "git-proxy: mirror ensure failed");
            return reject_status(StatusCode::BAD_GATEWAY, "mirror_unavailable");
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
    let bytes_received = req_in.body.len() as u64;
    tracing::Span::current().record("bytes_received", bytes_received);

    let cgi_resp = match spawn_http_backend(&mirror_path, &req_in).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "git-proxy: http-backend spawn failed");
            return reject_status(StatusCode::BAD_GATEWAY, "backend_spawn_failed");
        }
    };

    let bytes_sent = cgi_resp.body.len() as u64;
    let upstream_status = cgi_resp.status.as_u16();
    tracing::Span::current().record("bytes_sent", bytes_sent);
    tracing::Span::current().record("upstream_status", upstream_status);

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

/// Sets up the global `Audit` context for a request — subject,
/// project_id (attribution), action, and metadata. Called once per
/// request as soon as the (auth, coord, service, refs) tuple is
/// known. Pair with [`finish_audit`] / [`finish_authz_reject`] /
/// [`finish_audit_reject`] which apply the outcome and persist.
fn seed_audit_context(auth: &AuthSubject, coord: &RepoCoord, service: GitService, refs: &[String]) {
    Audit::record_subject(auth);
    if let Some(project_id) = auth.project_id() {
        Audit::record_project_id(project_id);
    }
    if let Some(agent_id) = auth.acting_agent_id() {
        Audit::record_agent_id(agent_id);
    }
    Audit::record_entrypoint(format!("api: git-proxy.{}", service.as_str()));
    Audit::record_action(format!("git_proxy.{}", service.as_str()));
    Audit::record_metadata(serde_json::json!({
        "owner": coord.owner,
        "repo": coord.repo,
        "service": service.as_str(),
        "refs": refs,
    }));
}

/// Sets the audit outcome from the response status (the global
/// `audit_middleware` persists). 2xx → Success; anything else → Error
/// with the status code as the message (handler-side `reject_*`
/// helpers seed a more specific reject_code via
/// [`finish_authz_reject`] / [`finish_audit_reject`] first).
fn finish_audit(response: &Response) {
    let status = response.status();
    if status.is_success() {
        Audit::record_outcome_if_unset(InteractionOutcome::Success);
    } else {
        Audit::record_outcome_if_unset(InteractionOutcome::Error {
            message: status.as_u16().to_string(),
        });
    }
}

/// Authorisation reject path. Maps the typed error to a status code,
/// records the audit outcome, and renders the response body. Allow-list
/// errors emit their full `detail()` so a denied push surfaces the
/// configured `allowed_push_refs` directly to the agent's stderr via
/// `git`'s `remote:` line prefix.
fn finish_authz_reject(err: GitProxyError) -> Response {
    let code = err.reject_code();
    tracing::warn!(error = %err, code = code, "git-proxy rejected request");
    let (status, body) = match &err {
        GitProxyError::Authorization(_) => (StatusCode::UNAUTHORIZED, format!("{code}\n")),
        GitProxyError::Allowlist(e) => {
            let status = match e {
                drua_core::git_proxy::AllowlistError::RepoNotAllowed { .. }
                | drua_core::git_proxy::AllowlistError::ModeNotAllowed { .. }
                | drua_core::git_proxy::AllowlistError::PushRefDenied { .. } => {
                    StatusCode::FORBIDDEN
                }
                drua_core::git_proxy::AllowlistError::InvalidRefPattern(_) => {
                    StatusCode::BAD_REQUEST
                }
            };
            (status, e.detail())
        }
        GitProxyError::InvalidRepoCoord { .. } => (StatusCode::BAD_REQUEST, format!("{code}\n")),
    };
    Audit::record_outcome(InteractionOutcome::Error {
        message: code.to_string(),
    });
    reject_with_body(status, &body)
}

/// Records a reject outcome with the given `reject_code` and renders
/// the HTTP response. Persistence happens in `audit_middleware`.
fn finish_audit_reject(status: StatusCode, code: &'static str) -> Response {
    Audit::record_outcome(InteractionOutcome::Error {
        message: code.to_string(),
    });
    reject_status(status, code)
}

/// Pre-context-seed reject — used for failures that happen before we
/// know enough about the request to seed an audit row (e.g. malformed
/// URL coord, body-read failure, missing service= query). Records a
/// minimal audit context from whatever subject we have so the rejection
/// is still visible.
fn reject_response(auth: &AuthSubject, status: StatusCode, code: &'static str) -> Response {
    Audit::record_subject(auth);
    if let Some(project_id) = auth.project_id() {
        Audit::record_project_id(project_id);
    }
    Audit::record_entrypoint("api: git-proxy");
    Audit::record_action("git_proxy.reject");
    Audit::record_outcome(InteractionOutcome::Error {
        message: code.to_string(),
    });
    reject_status(status, code)
}

fn reject_status(status: StatusCode, code: &'static str) -> Response {
    reject_with_body(status, &format!("{code}\n"))
}

/// Renders a multi-line text/plain rejection body with a `git-proxy: `
/// prefix on the first line. Used by [`finish_authz_reject`] to surface
/// allow-list detail (e.g. the configured `allowed_push_refs`) so a
/// denied push is self-explanatory in the agent's stderr.
fn reject_with_body(status: StatusCode, body: &str) -> Response {
    let prefixed = format!("git-proxy: {body}");
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        prefixed,
    )
        .into_response()
}
