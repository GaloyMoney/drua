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

use domain::auth::AuthContext;
use domain::mcp_creds::token::generate_token;
use domain::mcp_creds::McpCreds;
use domain::primitives::{AgentId, McpCredsId, UserId, WorkspaceSecretId};

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
        .route("/reports", get(reports_page))
        .route("/reports/list", get(reports_list))
        .route("/reports/search", get(reports_search))
        .route("/reports/{id}", get(reports_detail))
        .route("/workspaces/{id}/chat", get(workspace_chat))
        .route("/workspaces", get(workspaces_page))
        .route("/workspaces/new", get(workspace_new))
        .route("/workspaces/list", get(workspace_list))
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
        .route("/agents/{id}/config", get(agent_config_panel))
        .route("/agents/{id}/config", post(agent_config_update))
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
        vec!["admin".to_string()]
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

#[derive(Debug, Deserialize)]
pub struct AuditFilterParams {
    subject: Option<String>,
}

#[instrument(name = "web.audit_page", skip_all)]
async fn audit_page(session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    AuditTemplate {}.into_response()
}

#[instrument(name = "web.audit_entries", skip_all)]
async fn audit_entries(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<AuditFilterParams>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let audit = state.app.audit();
    let subject = params.subject.as_deref().filter(|s| !s.is_empty());

    let entries = match subject {
        Some(subj) => audit.find_by_subject(subj, 50).await,
        None => audit.list_recent(50).await,
    };

    match entries {
        Ok(entries) => {
            let mut views = Vec::with_capacity(entries.len());
            for entry in entries {
                let subject = resolve_subject(&state.app, &entry.subject).await;
                views.push(AuditEntryView {
                    subject,
                    action: entry.action,
                    outcome: entry.outcome,
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

async fn resolve_subject(app: &galoy_agents_core::App, subject: &str) -> AuditSubjectView {
    if let Some(creds_id_str) = subject.strip_prefix("exported_agent::") {
        if let Ok(creds_id) = creds_id_str.parse::<uuid::Uuid>() {
            let creds_id = galoy_agents_core::primitives::McpCredsId::from(creds_id);
            if let Ok(creds) = app.mcp_creds().find_by_id(creds_id).await {
                let owner_name = if let Some(user_id) = creds.owner.user_id() {
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
                        .unwrap_or_else(|| "unknown".to_string())
                } else {
                    "agent".to_string()
                };
                return AuditSubjectView {
                    label: creds.name,
                    owner: Some(owner_name),
                };
            }
        }
    } else if let Some(rest) = subject.strip_prefix("agent::") {
        // Format: agent::<agent-uuid>::ws:<workspace-uuid>
        if let Some((agent_id_str, ws_id_str)) = rest.split_once("::ws:") {
            let agent_label = if let Ok(agent_uuid) = agent_id_str.parse::<uuid::Uuid>() {
                let agent_id = galoy_agents_core::primitives::AgentId::from(agent_uuid);
                app.agents()
                    .find_by_id(agent_id)
                    .await
                    .map(|a| format_agent_type(&a.agent_type))
                    .unwrap_or_else(|_| agent_id_str.to_string())
            } else {
                agent_id_str.to_string()
            };

            let ws_name = if let Ok(ws_uuid) = ws_id_str.parse::<uuid::Uuid>() {
                let workspace_id = galoy_agents_core::primitives::WorkspaceId::from(ws_uuid);
                app.workspaces()
                    .find_by_id(workspace_id)
                    .await
                    .map(|ws| ws.name.clone())
                    .unwrap_or_else(|_| ws_id_str.to_string())
            } else {
                ws_id_str.to_string()
            };

            return AuditSubjectView {
                label: agent_label,
                owner: Some(ws_name),
            };
        }
    } else if let Some(user_id_str) = subject.strip_prefix("user::") {
        if let Ok(user_id) = user_id_str.parse::<uuid::Uuid>() {
            let user_id = galoy_agents_core::primitives::UserId::from(user_id);
            if let Ok(user) = app.users().find_by_id(user_id).await {
                let label = user
                    .name
                    .clone()
                    .or_else(|| user.email.clone())
                    .unwrap_or_else(|| user.github_id.clone());
                return AuditSubjectView { label, owner: None };
            }
        }
    } else if subject == "anonymous" {
        return AuditSubjectView {
            label: "anonymous".to_string(),
            owner: None,
        };
    }
    AuditSubjectView {
        label: subject.to_string(),
        owner: None,
    }
}

fn format_agent_type(agent_type: &galoy_agents_core::primitives::AgentType) -> String {
    match agent_type {
        galoy_agents_core::primitives::AgentType::WorkspaceLead => "Workspace Lead".to_string(),
    }
}

#[instrument(name = "web.code_assistant_dashboard", skip_all)]
async fn code_assistant_dashboard(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let ca = state.app.code_assistant().unwrap();

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

    let rows = match state
        .app
        .code_assistant()
        .unwrap()
        .logs()
        .recent_requests(10)
        .await
    {
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

    let rows = match state
        .app
        .code_assistant()
        .unwrap()
        .logs()
        .least_useful(10)
        .await
    {
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
// Reports
// ---------------------------------------------------------------------------

fn report_to_view(m: &domain::report::Report) -> ReportView {
    let id_str = m.id.to_string();
    ReportView {
        id: id_str.clone(),
        short_id: id_str[..8.min(id_str.len())].to_string(),
        title: m.title.clone(),
        content: m.content.clone(),
        tags: if m.tags.is_empty() {
            String::new()
        } else {
            m.tags.join(", ")
        },
        created_at: m.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        pinned: m.pinned,
    }
}

#[instrument(name = "web.reports_page", skip_all)]
async fn reports_page(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    ReportsTemplate {
        enabled: state.app.reports().is_some(),
    }
    .into_response()
}

#[instrument(name = "web.reports_list", skip_all)]
async fn reports_list(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let svc = match state.app.reports() {
        Some(svc) => svc,
        None => return ReportsListTemplate { reports: vec![] }.into_response(),
    };

    match svc.list(50).await {
        Ok(reports) => ReportsListTemplate {
            reports: reports.iter().map(report_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list reports");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReportsSearchParams {
    q: Option<String>,
}

#[instrument(name = "web.reports_search", skip_all)]
async fn reports_search(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<ReportsSearchParams>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let query = params.q.unwrap_or_default();
    if query.is_empty() {
        return ReportsSearchResultsTemplate {
            query,
            results: vec![],
            error: None,
        }
        .into_response();
    }

    let svc = match state.app.reports() {
        Some(svc) => svc,
        None => {
            return ReportsSearchResultsTemplate {
                query,
                results: vec![],
                error: Some("Report service is not enabled".to_string()),
            }
            .into_response();
        }
    };

    match svc.search(&query, 20).await {
        Ok(results) => {
            let views = results
                .iter()
                .map(|r| {
                    let id_str = r.id.to_string();
                    ReportSearchResultView {
                        id: id_str.clone(),
                        short_id: id_str[..8.min(id_str.len())].to_string(),
                        title: r.title.clone(),
                        content: r.content.clone(),
                        tags: if r.tags.is_empty() {
                            String::new()
                        } else {
                            r.tags.join(", ")
                        },
                        score: format!("{:.3}", r.score),
                        pinned: r.pinned,
                    }
                })
                .collect();
            ReportsSearchResultsTemplate {
                query,
                results: views,
                error: None,
            }
            .into_response()
        }
        Err(e) => ReportsSearchResultsTemplate {
            query,
            results: vec![],
            error: Some(e.to_string()),
        }
        .into_response(),
    }
}

#[instrument(name = "web.reports_detail", skip_all)]
async fn reports_detail(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<String>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let svc = match state.app.reports() {
        Some(svc) => svc,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    match svc.find_by_id_prefix(&id).await {
        Ok(Some(m)) => ReportDetailTemplate {
            report: report_to_view(&m),
        }
        .into_response(),
        Ok(None) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to find report by prefix");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

fn agent_to_config_view(agent: &domain::agent::Agent) -> AgentConfigView {
    AgentConfigView {
        model: agent.chat_config.model.clone(),
        max_tokens: agent.chat_config.max_tokens,
        max_turns: agent.chat_config.max_turns,
        resource_cpu: agent.sandbox_config.resource_cpu.clone(),
        resource_mem: agent.sandbox_config.resource_mem.clone(),
    }
}

fn workspace_to_view(ws: &domain::workspace::Workspace) -> WorkspaceView {
    WorkspaceView {
        id: ws.id.to_string(),
        name: ws.name.clone(),
        description: ws.description.clone().unwrap_or_default(),
        created_at: ws.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

#[instrument(name = "web.workspaces_page", skip_all)]
async fn workspaces_page(session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    WorkspacesTemplate {}.into_response()
}

#[instrument(name = "web.workspace_list", skip_all)]
async fn workspace_list(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    match state.app.workspaces().list_all().await {
        Ok(workspaces) => WorkspaceListTemplate {
            workspaces: workspaces.iter().map(workspace_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list workspaces");
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
        Ok(_) => Redirect::to("/workspaces").into_response(),
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

    let agents = state
        .app
        .agents()
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default();

    let agent_id = agents
        .iter()
        .find(|a| a.agent_type == domain::primitives::AgentType::WorkspaceLead)
        .map(|a| a.id.to_string())
        .unwrap_or_default();

    WorkspaceDetailTemplate {
        workspace: workspace_to_view(&ws),
        agent_id,
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
        Ok(_) => Redirect::to("/workspaces").into_response(),
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
// Workspace Chat
// ---------------------------------------------------------------------------

#[instrument(name = "web.workspace_chat", skip_all)]
async fn workspace_chat(
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

    let agents = state
        .app
        .agents()
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default();

    let lead = agents
        .iter()
        .find(|a| a.agent_type == domain::primitives::AgentType::WorkspaceLead);

    let agent_id = match lead {
        Some(a) => a.id.to_string(),
        None => return Redirect::to("/workspaces").into_response(),
    };

    WorkspaceChatTemplate {
        workspace: workspace_to_view(&ws),
        agent_id,
    }
    .into_response()
}

// ---------------------------------------------------------------------------
// Agent Config
// ---------------------------------------------------------------------------

#[instrument(name = "web.agent_config_panel", skip_all)]
async fn agent_config_panel(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let agent_id = AgentId::from(id);
    let agent = match state.app.agents().find_by_id(agent_id).await {
        Ok(a) => a,
        Err(_) => return axum::http::StatusCode::NOT_FOUND.into_response(),
    };

    AgentConfigPanelTemplate {
        agent_id: agent.id.to_string(),
        config: agent_to_config_view(&agent),
        saved: false,
    }
    .into_response()
}

#[derive(Deserialize)]
struct AgentConfigForm {
    model: String,
    max_tokens: u32,
    max_turns: u32,
    resource_cpu: String,
    resource_mem: String,
}

#[instrument(name = "web.agent_config_update", skip_all)]
async fn agent_config_update(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<AgentConfigForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let agent_id = AgentId::from(id);

    // Fetch current agent to preserve non-editable sandbox fields
    let current = match state.app.agents().find_by_id(agent_id).await {
        Ok(a) => a,
        Err(_) => return axum::http::StatusCode::NOT_FOUND.into_response(),
    };

    let chat_config = domain::agent::ChatConfig {
        model: form.model,
        max_tokens: form.max_tokens,
        max_turns: form.max_turns,
    };
    let sandbox_config = domain::agent::SandboxConfig {
        resource_cpu: form.resource_cpu,
        resource_mem: form.resource_mem,
        pvc_size: current.sandbox_config.pvc_size.clone(),
        disallowed_tools: current.sandbox_config.disallowed_tools.clone(),
    };

    match state
        .app
        .agents()
        .update_config(agent_id, chat_config, sandbox_config)
        .await
    {
        Ok(agent) => AgentConfigPanelTemplate {
            agent_id: agent.id.to_string(),
            config: agent_to_config_view(&agent),
            saved: true,
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update agent config");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
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
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<AgentMessageRequest>,
) -> Response {
    let user_id = match &auth {
        AuthContext::User(id) => *id,
        AuthContext::ExportedAgent(id, _, _) => *id,
        AuthContext::Agent(_, _, _) => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
        AuthContext::Anonymous => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };

    let agent_id = AgentId::from(id);
    let rx = match state
        .app
        .agents()
        .send_message(agent_id, user_id, body.prompt)
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
                domain::agent::AgentMessageEvent::Text { .. } => "text",
                domain::agent::AgentMessageEvent::ToolCall { .. } => "tool_call",
                domain::agent::AgentMessageEvent::ToolResult { .. } => "tool_result",
                domain::agent::AgentMessageEvent::Done { .. } => "done",
                domain::agent::AgentMessageEvent::Error { .. } => "error",
                domain::agent::AgentMessageEvent::Service { .. } => "service",
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
/// Secured via SA token auth (AuthContext::Agent) — same pattern as MCP gateway.
#[instrument(name = "api.agent.secrets", skip_all)]
async fn api_agent_secrets(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    // Only allow Agent auth (SA token from sandbox pods)
    let (workspace_id, jwt_agent_id) = match &auth {
        AuthContext::Agent(workspace_id, agent_id, _) => (*workspace_id, *agent_id),
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
    if let Some(github_app) = state.app.github_app() {
        match github_app.generate_token().await {
            Ok(token) => {
                body.push(serde_json::json!({
                    "name": "github-token",
                    "secret_type": "file",
                    "value": token.token,
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to generate GitHub App token — skipping");
            }
        }
    }

    Json(body).into_response()
}
