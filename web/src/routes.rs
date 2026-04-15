use std::convert::Infallible;

use axum::{
    extract::{Path, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Redirect, Response,
    },
    routing::{get, post},
    Extension, Form, Json, Router,
};
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;
use tower_sessions::Session;
use tracing::instrument;

use galoy_agents_core as domain;

use domain::auth::AuthSubject;
use domain::mcp_creds::token::generate_token;
use domain::mcp_creds::McpCreds;
use domain::primitives::{AgentId, McpCredsId, SandboxId, SkillId, UserId, WorkspaceSecretId};
use domain::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

use crate::templates::*;
use crate::AppState;

#[derive(Debug, Deserialize, Default)]
pub struct IndexParams {
    pub error: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/dashboard", get(dashboard))
        .route("/dashboard/mcp-creds", get(mcp_creds_list))
        .route("/mcp-creds/create", post(create_mcp_creds))
        .route("/mcp-creds/{id}/revoke", post(revoke_mcp_creds))
        .route("/audit", get(audit_page))
        .route("/audit/entries", get(audit_entries))
        .route("/code-assistant", get(code_assistant_dashboard))
        .route("/code-assistant/recent", get(code_assistant_recent))
        .route(
            "/code-assistant/least-useful",
            get(code_assistant_least_useful),
        )
        .route("/code-assistant/search", get(code_assistant_search))
        .route("/workspaces/{id}/chat", get(workspace_chat))
        .route("/workspaces", get(workspaces_page))
        .route("/workspaces/new", get(workspace_new))
        .route("/workspaces/sidebar", get(workspace_sidebar_list))
        .route("/workspaces", post(workspace_create))
        .route("/workspaces/{id}", get(workspace_detail))
        .route("/workspaces/{id}", post(workspace_update))
        .route("/workspaces/{id}/delete", post(workspace_delete))
        .route("/workspaces/{id}/secrets", post(workspace_secret_create))
        .route("/workspaces/{id}/secrets/list", get(workspace_secrets_list))
        .route(
            "/workspaces/{id}/secrets/{secret_id}/delete",
            post(workspace_secret_delete),
        )
        .route("/workspaces/{id}/skills", get(workspace_skills_page))
        .route("/workspaces/{id}/skills/new", get(workspace_skill_new))
        .route("/workspaces/{id}/skills", post(workspace_skill_create))
        .route(
            "/workspaces/{id}/skills/{skill_id}",
            get(workspace_skill_edit),
        )
        .route(
            "/workspaces/{id}/skills/{skill_id}",
            post(workspace_skill_update),
        )
        .route(
            "/workspaces/{id}/skills/{skill_id}/delete",
            post(workspace_skill_delete),
        )
        .route("/workspaces/{id}/sandboxes", get(workspace_sandboxes_page))
        .route("/workspaces/{id}/sandboxes/new", get(workspace_sandbox_new))
        .route("/workspaces/{id}/sandboxes", post(workspace_sandbox_create))
        .route(
            "/workspaces/{id}/sandboxes/{sandbox_id}",
            get(workspace_sandbox_detail),
        )
        .route(
            "/workspaces/{id}/sandboxes/{sandbox_id}/suspend",
            post(workspace_sandbox_suspend),
        )
        .route(
            "/workspaces/{id}/sandboxes/{sandbox_id}/restart",
            post(workspace_sandbox_restart),
        )
        .route("/workspaces/{id}/agents/new", get(workspace_agent_new))
        .route("/workspaces/{id}/agents", post(workspace_agent_create))
        .route(
            "/workspaces/{id}/agents/{agent_id}",
            get(workspace_agent_detail),
        )
        .route(
            "/workspaces/{id}/agents/{agent_id}/attach_sandbox",
            post(workspace_agent_attach_sandbox),
        )
        .route(
            "/workspaces/{id}/agents/{agent_id}/detach_sandbox",
            post(workspace_agent_detach_sandbox),
        )
}

async fn extract_user_id(session: &Session) -> Option<UserId> {
    session.get("user_id").await.ok()?
}

fn mcp_creds_to_view(creds: &McpCreds) -> McpCredsView {
    McpCredsView {
        id: creds.id.to_string(),
        name: creds.name.clone(),
        created_at: creds.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        is_revoked: creds.is_revoked(),
    }
}

#[instrument(name = "web.index", skip_all)]
async fn index(session: Session, Query(params): Query<IndexParams>) -> Response {
    if extract_user_id(&session).await.is_some() {
        return Redirect::to("/dashboard").into_response();
    }
    LoginTemplate {
        error: params.error,
    }
    .into_response()
}

#[instrument(name = "web.dashboard", skip_all)]
async fn dashboard(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let user = match state.app.users().find_by_id(user_id).await {
        Ok(user) => user,
        Err(_) => return Redirect::to("/").into_response(),
    };

    let mcp_creds = state
        .app
        .mcp_creds()
        .list_all_for_user(user.id)
        .await
        .unwrap_or_default();

    let user_name = user.name.clone().unwrap_or_else(|| user.github_id.clone());

    DashboardTemplate {
        user_name,
        mcp_creds: mcp_creds.iter().map(mcp_creds_to_view).collect(),
    }
    .into_response()
}

#[derive(serde::Deserialize)]
pub struct CreateMcpCredsForm {
    name: String,
    #[serde(default)]
    admin: Option<String>,
}

fn build_mcp_config(mcp_endpoint: &str, token: &str) -> (String, String) {
    let server_config = serde_json::json!({
        "type": "http",
        "url": mcp_endpoint,
        "headers": {
            "Authorization": format!("Bearer {token}")
        }
    });
    let mcp_json = serde_json::json!({
        "galoy-agents": &server_config
    })
    .to_string();
    let cli_command = format!(
        "claude mcp add-json --scope user galoy-agents '{}'",
        server_config
    );
    (mcp_json, cli_command)
}

#[instrument(name = "web.create_mcp_creds", skip_all)]
async fn create_mcp_creds(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<CreateMcpCredsForm>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let (raw_token, token_hash) = generate_token();

    let scopes = if form.admin.as_deref() == Some("true") {
        vec![domain::primitives::AuthScope::Admin]
    } else {
        vec![]
    };

    match state
        .app
        .mcp_creds()
        .create_for_user(user_id, &form.name, token_hash, scopes)
        .await
    {
        Ok(_) => {
            let (mcp_json, cli_command) = build_mcp_config(&state.mcp_endpoint, &raw_token);
            McpCredsCreatedTemplate {
                creds_name: form.name,
                mcp_json,
                cli_command,
            }
            .into_response()
        }
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.revoke_mcp_creds", skip_all)]
async fn revoke_mcp_creds(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let creds_id = McpCredsId::from(id);
    match state.app.mcp_creds().revoke(user_id, creds_id).await {
        Ok(creds) => McpCredsRowTemplate {
            creds: mcp_creds_to_view(&creds),
        }
        .into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.mcp_creds_list", skip_all)]
async fn mcp_creds_list(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let mcp_creds = state
        .app
        .mcp_creds()
        .list_all_for_user(user_id)
        .await
        .unwrap_or_default();

    McpCredsListTemplate {
        mcp_creds: mcp_creds.iter().map(mcp_creds_to_view).collect(),
    }
    .into_response()
}

#[instrument(name = "web.audit_page", skip_all)]
async fn audit_page(session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    AuditTemplate {}.into_response()
}

#[instrument(name = "web.audit_entries", skip_all)]
async fn audit_entries(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let entries = state.app.audit().list_recent(50).await;

    match entries {
        Ok(entries) => {
            let mut views = Vec::with_capacity(entries.len());
            for entry in entries {
                let acting_user = match entry.acting_user_id {
                    Some(id) => Some(lookup_user_label(&state.app, id).await),
                    None => None,
                };
                let workspace = match entry.workspace_id {
                    Some(id) => Some(lookup_workspace_label(&state.app, id).await),
                    None => None,
                };
                let acting_agent = match entry.acting_agent_id {
                    Some(id) => Some(lookup_agent_label(&state.app, id).await),
                    None => None,
                };
                let on_behalf_of = match entry.on_behalf_of_user_id {
                    Some(id) => Some(lookup_user_label(&state.app, id).await),
                    None => None,
                };
                views.push(AuditEntryView {
                    acting_user,
                    workspace,
                    acting_agent,
                    on_behalf_of,
                    action: entry.action,
                    outcome: entry.outcome,
                    error: entry.error.unwrap_or(false),
                    duration_ms: entry.duration_ms,
                    tokens_returned: entry.tokens_returned,
                    metadata: entry.metadata,
                    recorded_at: entry.recorded_at,
                });
            }
            AuditEntriesTemplate { entries: views }.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to load audit entries");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn lookup_user_label(
    app: &galoy_agents_core::App,
    user_id: galoy_agents_core::primitives::UserId,
) -> String {
    app.users()
        .find_by_id(user_id)
        .await
        .ok()
        .map(|u| {
            u.name
                .clone()
                .or_else(|| u.email.clone())
                .unwrap_or_else(|| u.github_id.clone())
        })
        .unwrap_or_else(|| user_id.to_string())
}

async fn lookup_agent_label(
    app: &galoy_agents_core::App,
    agent_id: galoy_agents_core::primitives::AgentId,
) -> String {
    app.agents()
        .find_by_id(agent_id)
        .await
        .map(|a| a.name.clone())
        .unwrap_or_else(|_| agent_id.to_string())
}

async fn lookup_workspace_label(
    app: &galoy_agents_core::App,
    workspace_id: galoy_agents_core::primitives::WorkspaceId,
) -> String {
    app.workspaces()
        .find_by_id(workspace_id)
        .await
        .map(|ws| ws.name.clone())
        .unwrap_or_else(|_| workspace_id.to_string())
}

#[instrument(name = "web.code_assistant_dashboard", skip_all)]
async fn code_assistant_dashboard(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let ca = match state.app.code_assistant() {
        Some(ca) => ca,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let stats = match ca.logs().dashboard_stats().await {
        Ok(stats) => stats,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load code assistant stats");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let label_origins = match ca.label_origin_counts() {
        Ok(counts) => counts,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load label origin counts");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    CodeAssistantTemplate {
        stats,
        label_origins,
    }
    .into_response()
}

#[instrument(name = "web.code_assistant_recent", skip_all)]
async fn code_assistant_recent(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let ca = match state.app.code_assistant() {
        Some(ca) => ca,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let rows = match ca.logs().recent_requests(10).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load recent requests");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    CodeAssistantRecentTemplate { rows }.into_response()
}

#[instrument(name = "web.code_assistant_least_useful", skip_all)]
async fn code_assistant_least_useful(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let ca = match state.app.code_assistant() {
        Some(ca) => ca,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let rows = match ca.logs().least_useful(10).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load least useful requests");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    CodeAssistantRecentTemplate { rows }.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    label: Option<String>,
}

#[instrument(name = "web.code_assistant_search", skip_all)]
async fn code_assistant_search(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<SearchParams>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let query = params.q.unwrap_or_default();
    if query.is_empty() {
        return CodeAssistantSearchResultsTemplate {
            query,
            results: vec![],
            error: None,
        }
        .into_response();
    }

    let endpoints = match state.code_assistant.as_ref() {
        Some(ep) => ep,
        None => {
            return CodeAssistantSearchResultsTemplate {
                query,
                results: vec![],
                error: Some("Code assistant is not configured".to_string()),
            }
            .into_response();
        }
    };

    let label = params.label.as_deref().filter(|s| !s.is_empty());

    match endpoints.search_raw(&query, 10, label).await {
        Ok(raw_results) => {
            let results = raw_results
                .iter()
                .map(|r| SearchResultView {
                    file_path: r.file_path.clone(),
                    repo: r.repo.clone(),
                    score: format!("{:.3}", r.score),
                    labels: r.labels.join(", "),
                    lines: format!("{}-{}", r.line_start, r.line_end),
                    content: r.content.clone(),
                    language: if r.language.is_empty() {
                        "rust".to_string()
                    } else {
                        r.language.clone()
                    },
                })
                .collect();
            CodeAssistantSearchResultsTemplate {
                query,
                results,
                error: None,
            }
            .into_response()
        }
        Err(e) => CodeAssistantSearchResultsTemplate {
            query,
            results: vec![],
            error: Some(e.to_string()),
        }
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

fn workspace_to_view(ws: &domain::workspace::Workspace) -> WorkspaceView {
    WorkspaceView {
        id: ws.id.to_string(),
        name: ws.name.clone(),
        description: ws.description.clone().unwrap_or_default(),
        created_at: ws.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

#[instrument(name = "web.workspaces_page", skip_all)]
async fn workspaces_page(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspaces = state.app.workspaces().list_all().await.unwrap_or_default();

    WorkspaceHubTemplate {
        workspaces: workspaces.iter().map(workspace_to_view).collect(),
        selected_workspace: None,
        selected_workspace_id: String::new(),
        lead_agent: None,
        agents: vec![],
        selected_agent_id: String::new(),
        selected_agent: None,
    }
    .into_response()
}

#[instrument(name = "web.workspace_sidebar_list", skip_all)]
async fn workspace_sidebar_list(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    match state.app.workspaces().list_all().await {
        Ok(workspaces) => WorkspaceSidebarListTemplate {
            workspaces: workspaces.iter().map(workspace_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list workspaces for sidebar");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "web.workspace_new", skip_all)]
async fn workspace_new(session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    WorkspaceNewTemplate {}.into_response()
}

#[derive(serde::Deserialize)]
pub struct WorkspaceForm {
    name: String,
    description: Option<String>,
}

#[instrument(name = "web.workspace_create", skip_all)]
async fn workspace_create(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<WorkspaceForm>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let description = form.description.filter(|d| !d.is_empty());
    match state
        .app
        .workspaces()
        .create(user_id, &form.name, description)
        .await
    {
        Ok(ws) => Redirect::to(&format!("/workspaces/{}/chat", ws.id)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create workspace");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "web.workspace_detail", skip_all)]
async fn workspace_detail(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    WorkspaceDetailTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
    }
    .into_response()
}

#[instrument(name = "web.workspace_update", skip_all)]
async fn workspace_update(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<WorkspaceForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let description = form.description.filter(|d| !d.is_empty());
    match state
        .app
        .workspaces()
        .update(workspace_id, &form.name, description)
        .await
    {
        Ok(_) => Redirect::to(&format!("/workspaces/{id}")).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update workspace");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "web.workspace_delete", skip_all)]
async fn workspace_delete(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    headers: axum::http::HeaderMap,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    match state.app.workspaces().delete(workspace_id).await {
        Ok(_) => {
            // If called via HTMX, redirect client-side using HX-Redirect
            if headers.contains_key("hx-request") {
                return (
                    [(
                        axum::http::header::HeaderName::from_static("hx-redirect"),
                        "/workspaces".parse::<axum::http::HeaderValue>().unwrap(),
                    )],
                    "",
                )
                    .into_response();
            }
            Redirect::to("/workspaces").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete workspace");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace Secrets
// ---------------------------------------------------------------------------

fn secret_to_view(s: &domain::workspace_secret::WorkspaceSecret) -> WorkspaceSecretView {
    WorkspaceSecretView {
        id: s.id.to_string(),
        workspace_id: s.workspace_id.to_string(),
        name: s.name.clone(),
        secret_type: s.secret_type.to_string(),
        updated_at: s.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

#[instrument(name = "web.workspace_secrets_list", skip_all)]
async fn workspace_secrets_list(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    match state
        .app
        .workspace_secrets()
        .list_by_workspace(workspace_id)
        .await
    {
        Ok(secrets) => WorkspaceSecretsListTemplate {
            secrets: secrets.iter().map(secret_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list workspace secrets");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct CreateSecretForm {
    name: String,
    secret_type: String,
    value: String,
}

#[instrument(name = "web.workspace_secret_create", skip_all)]
async fn workspace_secret_create(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<CreateSecretForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let secret_type: domain::workspace_secret::SecretType = match form.secret_type.parse() {
        Ok(t) => t,
        Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
    };

    if let Err(e) = state
        .app
        .workspace_secrets()
        .create(workspace_id, &form.name, secret_type, &form.value)
        .await
    {
        tracing::error!(error = %e, "Failed to create workspace secret");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return updated list
    match state
        .app
        .workspace_secrets()
        .list_by_workspace(workspace_id)
        .await
    {
        Ok(secrets) => WorkspaceSecretsListTemplate {
            secrets: secrets.iter().map(secret_to_view).collect(),
        }
        .into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.workspace_secret_delete", skip_all)]
async fn workspace_secret_delete(
    State(state): State<AppState>,
    session: Session,
    Path((id, secret_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let secret_id = WorkspaceSecretId::from(secret_id);

    if let Err(e) = state.app.workspace_secrets().delete(secret_id).await {
        tracing::error!(error = %e, "Failed to delete workspace secret");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return updated list
    match state
        .app
        .workspace_secrets()
        .list_by_workspace(workspace_id)
        .await
    {
        Ok(secrets) => WorkspaceSecretsListTemplate {
            secrets: secrets.iter().map(secret_to_view).collect(),
        }
        .into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Workspace Skills
// ---------------------------------------------------------------------------

fn skill_to_view(s: &domain::skill::Skill) -> SkillView {
    SkillView {
        id: s.id.to_string(),
        workspace_id: s.workspace_id.to_string(),
        name: s.name.clone(),
        description: s.description.clone(),
        body: s.body.clone(),
        created_at: s.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

#[instrument(name = "web.workspace_skills_page", skip_all)]
async fn workspace_skills_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    let skills = state
        .app
        .skills()
        .list_by_workspace_id(workspace_id)
        .await
        .unwrap_or_default();

    WorkspaceSkillsPageTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
        skills: skills.iter().map(skill_to_view).collect(),
    }
    .into_response()
}

#[instrument(name = "web.workspace_skill_new", skip_all)]
async fn workspace_skill_new(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    WorkspaceSkillNewTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
    }
    .into_response()
}

#[derive(Deserialize)]
struct CreateSkillForm {
    name: String,
    description: String,
    body: String,
}

#[instrument(name = "web.workspace_skill_create", skip_all)]
async fn workspace_skill_create(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<CreateSkillForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let new_skill = domain::skill::NewSkill::builder()
        .workspace_id(workspace_id)
        .name(form.name)
        .description(form.description)
        .body(form.body)
        .build()
        .expect("Could not build new skill");

    if let Err(e) = state.app.skills().create(new_skill).await {
        tracing::error!(error = %e, "Failed to create skill");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to(&format!("/workspaces/{id}/skills")).into_response()
}

#[instrument(name = "web.workspace_skill_edit", skip_all)]
async fn workspace_skill_edit(
    State(state): State<AppState>,
    session: Session,
    Path((id, skill_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let skill_id = SkillId::from(skill_id);
    let skill = match state.app.skills().find_by_id(skill_id).await {
        Ok(s) => s,
        Err(_) => return Redirect::to(&format!("/workspaces/{id}/skills")).into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    WorkspaceSkillEditTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
        skill: skill_to_view(&skill),
    }
    .into_response()
}

#[derive(Deserialize)]
struct UpdateSkillForm {
    name: String,
    description: String,
    body: String,
}

#[instrument(name = "web.workspace_skill_update", skip_all)]
async fn workspace_skill_update(
    State(state): State<AppState>,
    session: Session,
    Path((id, skill_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Form(form): Form<UpdateSkillForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let skill_id = SkillId::from(skill_id);
    let mut skill = match state.app.skills().find_by_id(skill_id).await {
        Ok(s) => s,
        Err(_) => return Redirect::to(&format!("/workspaces/{id}/skills")).into_response(),
    };

    if skill
        .update(Some(form.name), Some(form.description), Some(form.body))
        .did_execute()
    {
        if let Err(e) = state.app.skills().update(&mut skill).await {
            tracing::error!(error = %e, "Failed to update skill");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    Redirect::to(&format!("/workspaces/{id}/skills")).into_response()
}

#[instrument(name = "web.workspace_skill_delete", skip_all)]
async fn workspace_skill_delete(
    State(state): State<AppState>,
    session: Session,
    Path((id, skill_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let skill_id = SkillId::from(skill_id);

    if let Err(e) = state.app.skills().delete(skill_id).await {
        tracing::error!(error = %e, "Failed to delete skill");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to(&format!("/workspaces/{id}/skills")).into_response()
}

// ---------------------------------------------------------------------------
// Workspace Sandboxes
// ---------------------------------------------------------------------------

fn sandbox_to_view(s: &domain::sandbox::Sandbox) -> SandboxView {
    let (mode_label, repo_url) = match &s.mode {
        SandboxMode::Scratch => ("Scratch".to_string(), None),
        SandboxMode::Repo { repo_url } => ("Repo".to_string(), Some(repo_url.clone())),
    };

    let exported_system_prompt = s.exported_system_prompt.as_ref().map(|f| ExportedFileView {
        file_name: f.file_name.clone(),
        content: f.content.clone(),
    });
    let exported_skills: Vec<ExportedSkillView> = s
        .exported_skills
        .iter()
        .map(|sk| ExportedSkillView {
            name: sk.name.clone(),
            content: sk.content.clone(),
        })
        .collect();

    let exports_summary = match (exported_system_prompt.is_some(), exported_skills.len()) {
        (false, 0) => "—".to_string(),
        (true, 0) => "system prompt".to_string(),
        (false, 1) => "1 skill".to_string(),
        (false, n) => format!("{n} skills"),
        (true, 1) => "system prompt + 1 skill".to_string(),
        (true, n) => format!("system prompt + {n} skills"),
    };

    SandboxView {
        id: s.id.to_string(),
        workspace_id: s.workspace_id.to_string(),
        name: s.name.clone(),
        state: s.state.to_string(),
        last_error: s.last_error.clone(),
        mode_label,
        repo_url,
        cpu: s.specs.cpu.clone(),
        memory: s.specs.memory.clone(),
        disk_size: s.specs.disk_size.clone(),
        created_at: s.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        exports_summary,
        exported_system_prompt,
        exported_skills,
    }
}

/// Returns `(lead_agent, other_agents)` for the workspace sidebar.
/// The lead sits on its own above the `Agents` header; the rest form the
/// list underneath. If there's no WorkspaceLead (shouldn't happen after
/// workspace create, but handle gracefully), `lead_agent` is `None`.
async fn workspace_sidebar_context(
    state: &AppState,
    workspace_id: domain::primitives::WorkspaceId,
) -> (Option<AgentView>, Vec<AgentView>) {
    let agents = state
        .app
        .agents()
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default();

    let mut lead: Option<AgentView> = None;
    let mut others: Vec<AgentView> = Vec::with_capacity(agents.len());
    for a in agents.iter() {
        let view = AgentView {
            id: a.id.to_string(),
            name: a.name.clone(),
        };
        if a.agent_role == domain::agent::AgentRole::WorkspaceLead && lead.is_none() {
            lead = Some(view);
        } else {
            others.push(view);
        }
    }
    (lead, others)
}

#[instrument(name = "web.workspace_sandboxes_page", skip_all)]
async fn workspace_sandboxes_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    let sandboxes = state
        .app
        .sandboxes()
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default();

    WorkspaceSandboxesPageTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
        sandboxes: sandboxes.iter().map(sandbox_to_view).collect(),
    }
    .into_response()
}

#[instrument(name = "web.workspace_sandbox_new", skip_all)]
async fn workspace_sandbox_new(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    WorkspaceSandboxNewTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
    }
    .into_response()
}

#[derive(Deserialize)]
struct CreateSandboxForm {
    name: String,
    mode: String,
    #[serde(default)]
    repo_url: Option<String>,
    cpu: String,
    memory: String,
    disk_size: String,
}

#[instrument(name = "web.workspace_sandbox_create", skip_all)]
async fn workspace_sandbox_create(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<CreateSandboxForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);

    let mode = match form.mode.as_str() {
        "scratch" => SandboxMode::Scratch,
        "repo" => {
            let repo_url = form
                .repo_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            match repo_url {
                Some(repo_url) => SandboxMode::Repo { repo_url },
                None => {
                    tracing::warn!("repo mode requires a non-empty repo_url");
                    return Redirect::to(&format!("/workspaces/{id}/sandboxes/new"))
                        .into_response();
                }
            }
        }
        other => {
            tracing::warn!(mode = %other, "unknown sandbox mode");
            return Redirect::to(&format!("/workspaces/{id}/sandboxes/new")).into_response();
        }
    };

    let specs = SandboxSpecs {
        cpu: form.cpu,
        memory: form.memory,
        disk_size: form.disk_size,
    };

    if let Err(e) = state
        .app
        .sandboxes()
        .create(workspace_id, form.name, specs, mode)
        .await
    {
        tracing::error!(error = %e, "Failed to create sandbox");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to(&format!("/workspaces/{id}/sandboxes")).into_response()
}

#[instrument(name = "web.workspace_sandbox_detail", skip_all)]
async fn workspace_sandbox_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, sandbox_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let sandbox_id = SandboxId::from(sandbox_id);
    let sandbox = match state.app.sandboxes().find_by_id(sandbox_id).await {
        Ok(s) => s,
        Err(_) => return Redirect::to(&format!("/workspaces/{id}/sandboxes")).into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    WorkspaceSandboxDetailTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
        sandbox: sandbox_to_view(&sandbox),
    }
    .into_response()
}

#[instrument(name = "web.workspace_sandbox_suspend", skip_all)]
async fn workspace_sandbox_suspend(
    State(state): State<AppState>,
    session: Session,
    Path((id, sandbox_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let sandbox_id = SandboxId::from(sandbox_id);
    if let Err(e) = state.app.sandboxes().suspend(sandbox_id).await {
        tracing::error!(error = %e, "Failed to suspend sandbox");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to(&format!("/workspaces/{id}/sandboxes/{sandbox_id}")).into_response()
}

#[instrument(name = "web.workspace_sandbox_restart", skip_all)]
async fn workspace_sandbox_restart(
    State(state): State<AppState>,
    session: Session,
    Path((id, sandbox_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let sandbox_id = SandboxId::from(sandbox_id);
    if let Err(e) = state.app.sandboxes().restart(sandbox_id).await {
        tracing::error!(error = %e, "Failed to restart sandbox");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to(&format!("/workspaces/{id}/sandboxes/{sandbox_id}")).into_response()
}

// ---------------------------------------------------------------------------
// Workspace Agents (create form)
// ---------------------------------------------------------------------------

#[instrument(name = "web.workspace_agent_new", skip_all)]
async fn workspace_agent_new(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let (lead_agent, agent_views) = workspace_sidebar_context(&state, workspace_id).await;

    let sandbox_options: Vec<SandboxOptionView> = state
        .app
        .sandboxes()
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|s| SandboxOptionView {
            id: s.id.to_string(),
            name: s.name.clone(),
            state: s.state.to_string(),
        })
        .collect();

    WorkspaceAgentNewTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents: agent_views,
        sandbox_options,
    }
    .into_response()
}

#[derive(Deserialize)]
struct CreateAgentForm {
    name: String,
    /// Empty string when no sandbox selected; parsed as UUID otherwise.
    #[serde(default)]
    sandbox_id: Option<String>,
    #[serde(default)]
    attach_mode: Option<String>,
}

#[instrument(name = "web.workspace_agent_create", skip_all)]
async fn workspace_agent_create(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<CreateAgentForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);

    // Pair sandbox_id + attach_mode into Some((id, mode)) only when both are
    // present and valid; otherwise treat as "no attachment".
    let attach = form
        .sandbox_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|sb| sb.parse::<uuid::Uuid>().ok())
        .map(SandboxId::from)
        .map(|sb| {
            let mode = match form.attach_mode.as_deref().unwrap_or("read") {
                "write" => SandboxAgentMode::Write,
                _ => SandboxAgentMode::Read,
            };
            (sb, mode)
        });

    match state
        .app
        .agents()
        .create(
            workspace_id,
            domain::agent::AgentRole::Agent,
            form.name,
            attach,
        )
        .await
    {
        Ok(agent) => {
            Redirect::to(&format!("/workspaces/{id}/chat?agent={}", agent.id)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to create agent");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn role_label(role: domain::agent::AgentRole) -> &'static str {
    match role {
        domain::agent::AgentRole::WorkspaceLead => "Workspace Lead",
        domain::agent::AgentRole::Agent => "Agent",
    }
}

async fn attached_sandbox_view(
    state: &AppState,
    attached: Option<(SandboxId, SandboxAgentMode)>,
) -> Option<AttachedSandboxView> {
    let (sbx_id, mode) = attached?;
    let sandbox = state.app.sandboxes().find_by_id(sbx_id).await.ok()?;
    Some(AttachedSandboxView {
        id: sandbox.id.to_string(),
        name: sandbox.name.clone(),
        mode: match mode {
            SandboxAgentMode::Read => "read".to_string(),
            SandboxAgentMode::Write => "write".to_string(),
        },
        state: sandbox.state.to_string(),
    })
}

#[derive(Debug, Deserialize, Default)]
struct AgentDetailQuery {
    #[serde(default)]
    error: Option<String>,
}

#[instrument(name = "web.workspace_agent_detail", skip_all)]
async fn workspace_agent_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, agent_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Query(query): Query<AgentDetailQuery>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let agent_id = AgentId::from(agent_id);
    let agent = match state.app.agents().find_by_id(agent_id).await {
        Ok(a) => a,
        Err(_) => return Redirect::to(&format!("/workspaces/{id}")).into_response(),
    };

    let (lead_agent, agents) = workspace_sidebar_context(&state, workspace_id).await;

    let attached_view = attached_sandbox_view(&state, agent.attached_sandbox).await;

    // Dropdown options are only needed when no sandbox is currently attached.
    let sandbox_options: Vec<SandboxOptionView> = if attached_view.is_some() {
        Vec::new()
    } else {
        state
            .app
            .sandboxes()
            .list_for_workspace(workspace_id)
            .await
            .unwrap_or_default()
            .iter()
            .map(|s| SandboxOptionView {
                id: s.id.to_string(),
                name: s.name.clone(),
                state: s.state.to_string(),
            })
            .collect()
    };

    WorkspaceAgentDetailTemplate {
        workspace: workspace_to_view(&ws),
        lead_agent,
        agents,
        agent: AgentDetailView {
            id: agent.id.to_string(),
            workspace_id: agent.workspace_id.to_string(),
            name: agent.name.clone(),
            role: role_label(agent.agent_role).to_string(),
            is_lead: matches!(agent.agent_role, domain::agent::AgentRole::WorkspaceLead),
            attached_sandbox: attached_view,
        },
        sandbox_options,
        error: query.error,
    }
    .into_response()
}

/// Build `/workspaces/{id}/agents/{agent_id}?error=<encoded>` so the
/// detail page can render a flash banner explaining why the last
/// attach/detach failed.
fn agent_detail_redirect_with_error(
    workspace_id: uuid::Uuid,
    agent_id: AgentId,
    msg: &str,
) -> Response {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("error", msg)
        .finish();
    Redirect::to(&format!(
        "/workspaces/{workspace_id}/agents/{agent_id}?{query}"
    ))
    .into_response()
}

#[derive(Deserialize)]
struct AttachSandboxForm {
    sandbox_id: String,
    mode: String,
}

#[instrument(name = "web.workspace_agent_attach_sandbox", skip_all)]
async fn workspace_agent_attach_sandbox(
    State(state): State<AppState>,
    session: Session,
    Path((id, agent_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Form(form): Form<AttachSandboxForm>,
) -> Response {
    let Some(user_id) = extract_user_id(&session).await else {
        return Redirect::to("/").into_response();
    };
    let subject = domain::primitives::AuthSubject::User(user_id);

    let agent_id = AgentId::from(agent_id);
    let Ok(sandbox_uuid) = form.sandbox_id.parse::<uuid::Uuid>() else {
        tracing::warn!(raw = %form.sandbox_id, "invalid sandbox_id in attach form");
        return Redirect::to(&format!("/workspaces/{id}/agents/{agent_id}")).into_response();
    };
    let sandbox_id = SandboxId::from(sandbox_uuid);
    let mode = match form.mode.as_str() {
        "write" => SandboxAgentMode::Write,
        _ => SandboxAgentMode::Read,
    };

    if let Err(e) = state
        .app
        .agents()
        .attach_sandbox(&subject, agent_id, sandbox_id, mode)
        .await
    {
        tracing::warn!(error = %e, "attach_sandbox failed");
        return agent_detail_redirect_with_error(id, agent_id, &e.to_string());
    }

    Redirect::to(&format!("/workspaces/{id}/agents/{agent_id}")).into_response()
}

#[instrument(name = "web.workspace_agent_detach_sandbox", skip_all)]
async fn workspace_agent_detach_sandbox(
    State(state): State<AppState>,
    session: Session,
    Path((id, agent_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    let Some(user_id) = extract_user_id(&session).await else {
        return Redirect::to("/").into_response();
    };
    let subject = domain::primitives::AuthSubject::User(user_id);

    let agent_id = AgentId::from(agent_id);
    // The handler needs the sandbox_id; fetch the agent to look it up.
    let sandbox_id = match state.app.agents().find_by_id(agent_id).await {
        Ok(a) => a.attached_sandbox.map(|(sbx, _)| sbx),
        Err(_) => return Redirect::to(&format!("/workspaces/{id}")).into_response(),
    };
    let Some(sandbox_id) = sandbox_id else {
        // Already detached; nothing to do.
        return Redirect::to(&format!("/workspaces/{id}/agents/{agent_id}")).into_response();
    };

    if let Err(e) = state
        .app
        .agents()
        .detach_sandbox(&subject, agent_id, sandbox_id)
        .await
    {
        tracing::warn!(error = %e, "detach_sandbox failed");
        return agent_detail_redirect_with_error(id, agent_id, &e.to_string());
    }

    Redirect::to(&format!("/workspaces/{id}/agents/{agent_id}")).into_response()
}

// ---------------------------------------------------------------------------
// Workspace Chat
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ChatQuery {
    agent: Option<uuid::Uuid>,
}

#[instrument(name = "web.workspace_chat", skip_all)]
async fn workspace_chat(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Query(query): Query<ChatQuery>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let workspace_id = domain::primitives::WorkspaceId::from(id);
    let ws = match state.app.workspaces().find_by_id(workspace_id).await {
        Ok(ws) => ws,
        Err(_) => return Redirect::to("/workspaces").into_response(),
    };

    let all_workspaces = state.app.workspaces().list_all().await.unwrap_or_default();

    let agents = state
        .app
        .agents()
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default();

    let selected_agent = match query.agent {
        Some(agent_uuid) => {
            let target = AgentId::from(agent_uuid);
            agents.iter().find(|a| a.id == target)
        }
        None => agents
            .iter()
            .find(|a| a.agent_role == domain::agent::AgentRole::WorkspaceLead),
    };

    let selected_agent_view = selected_agent.map(|a| AgentView {
        id: a.id.to_string(),
        name: a.name.clone(),
    });
    let selected_agent_id = selected_agent_view
        .as_ref()
        .map(|v| v.id.clone())
        .unwrap_or_default();

    // Split lead out so the sidebar can render it above the "Agents" header.
    let mut lead_agent: Option<AgentView> = None;
    let mut agent_views: Vec<AgentView> = Vec::with_capacity(agents.len());
    for a in agents.iter() {
        let view = AgentView {
            id: a.id.to_string(),
            name: a.name.clone(),
        };
        if a.agent_role == domain::agent::AgentRole::WorkspaceLead && lead_agent.is_none() {
            lead_agent = Some(view);
        } else {
            agent_views.push(view);
        }
    }

    WorkspaceHubTemplate {
        workspaces: all_workspaces.iter().map(workspace_to_view).collect(),
        selected_workspace_id: workspace_id.to_string(),
        selected_workspace: Some(workspace_to_view(&ws)),
        lead_agent,
        agents: agent_views,
        selected_agent_id,
        selected_agent: selected_agent_view,
    }
    .into_response()
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/agents/{id}/message", post(api_agent_message))
        .route("/api/v1/agents/{id}/secrets", get(api_agent_secrets))
}

#[derive(Deserialize)]
struct AgentMessageRequest {
    prompt: String,
}

#[instrument(name = "api.agent.message", skip_all)]
async fn api_agent_message(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<AgentMessageRequest>,
) -> Response {
    // Caller must carry an originating-user identity (User or ExportedAgent).
    // Plain Agent / Anonymous tokens can't use this endpoint.
    if !matches!(
        auth,
        AuthSubject::User(_) | AuthSubject::ExportedAgent(_, _, _)
    ) {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let agent_id = AgentId::from(id);

    let rx = match state
        .app
        .agents()
        .send_message(auth, agent_id, body.prompt)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!(error = %e, "Failed to send message to agent");
            let body = serde_json::json!({ "error": e.to_string() });
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response();
        }
    };

    let (tx, sse_rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);

    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            let event_name = match &event {
                domain::primitives::ChatOutputEvent::UserMessage { .. } => "user_message",
                domain::primitives::ChatOutputEvent::AssistantText { .. } => "assistant_text",
                domain::primitives::ChatOutputEvent::Thinking { .. } => "thinking",
                domain::primitives::ChatOutputEvent::ToolCall { .. } => "tool_call",
                domain::primitives::ChatOutputEvent::ToolResult { .. } => "tool_result",
                domain::primitives::ChatOutputEvent::AssistantDone { .. } => "assistant_done",
                domain::primitives::ChatOutputEvent::Error { .. } => "error",
                domain::primitives::ChatOutputEvent::Service { .. } => "service",
            };
            let data = serde_json::to_string(&event).unwrap_or_default();
            let sse_event = Event::default().event(event_name).data(data);
            if tx.send(Ok(sse_event)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(sse_rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// Internal API — Agent Secrets (for harness injection)
// ---------------------------------------------------------------------------

/// Internal endpoint: returns secret values for an agent's workspace.
/// Secured via SA token auth (AuthSubject::Agent) — same pattern as MCP gateway.
#[instrument(
    name = "api.agent.secrets",
    skip_all,
    fields(github_token_provisioned, secret_count)
)]
async fn api_agent_secrets(
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    // Only allow Agent auth (SA token from sandbox pods)
    let (workspace_id, jwt_agent_id) = match &auth {
        AuthSubject::Agent(workspace_id, agent_id, _) => (*workspace_id, *agent_id),
        _ => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };

    // The path agent ID must match the one embedded in the JWT
    let path_agent_id = AgentId::from(id);
    if path_agent_id != jwt_agent_id {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let secrets = match state
        .app
        .workspace_secrets()
        .list_decrypted(workspace_id)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to list secrets for agent");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut body: Vec<serde_json::Value> = secrets
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "secret_type": s.secret_type.as_str(),
                "value": s.value,
            })
        })
        .collect();

    // Auto-provision a GitHub token if the GitHub App is configured.
    let github_token_provisioned = if let Some(github_app) = state.app.github_app() {
        match github_app.generate_token().await {
            Ok(token) => {
                tracing::info!("GitHub App token generated successfully");
                body.push(serde_json::json!({
                    "name": "github-token",
                    "secret_type": "file",
                    "value": token.token,
                }));
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to generate GitHub App token — skipping");
                false
            }
        }
    } else {
        tracing::info!("GitHub App not configured — skipping token provisioning");
        false
    };

    tracing::Span::current().record("github_token_provisioned", github_token_provisioned);
    tracing::Span::current().record("secret_count", body.len());

    Json(body).into_response()
}
