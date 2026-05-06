use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Extension, Form, Json, Router,
};
use axum_extra::extract::Query as RepeatedQuery;
use serde::Deserialize;
use tower_sessions::Session;
use tracing::instrument;

use drua_core as domain;

use domain::auth::AuthSubject;
use domain::mcp_creds::McpCreds;
use domain::primitives::{
    AgentId, SandboxId, SkillId, SpaceId, UserId, WorkflowDefinitionId, WorkflowRunId,
};
use domain::sandbox::{SandboxAgentMode, SandboxMode};
use domain::workflow::WorkflowTrigger;

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
        .route("/audit", get(audit_page))
        .route("/audit/entries", get(audit_entries))
        .route("/library", get(library_page))
        .route("/library/search", get(library_search))
        .route("/code-assistant", get(code_assistant_dashboard))
        .route("/code-assistant/recent", get(code_assistant_recent))
        .route(
            "/code-assistant/least-useful",
            get(code_assistant_least_useful),
        )
        .route("/code-assistant/search", get(code_assistant_search))
        .route("/projects/{id}/chat", get(project_chat))
        .route("/projects", get(projects_page))
        .route("/projects/new", get(project_new))
        .route("/projects/sidebar", get(project_sidebar_list))
        .route("/projects/{id}", get(project_detail))
        .route("/projects/{id}/secrets/list", get(project_secrets_list))
        .route("/projects/{id}/skills", get(project_skills_page))
        .route(
            "/projects/{id}/skills/{skill_id}",
            get(project_skill_detail),
        )
        .route("/projects/{id}/sandboxes", get(project_sandboxes_page))
        .route("/projects/{id}/sandboxes/new", get(project_sandbox_new))
        .route(
            "/projects/{id}/sandboxes/{sandbox_id}",
            get(project_sandbox_detail),
        )
        .route("/projects/{id}/agents/new", get(project_agent_new))
        .route(
            "/projects/{id}/agents/{agent_id}",
            get(project_agent_detail),
        )
        .route(
            "/projects/{id}/agents/{agent_id}/history",
            get(project_agent_history),
        )
        .route("/projects/{id}/spaces", get(project_spaces_page))
        .route("/projects/{id}/spaces/mount", post(project_space_mount))
        .route("/projects/{id}/spaces/unmount", post(project_space_unmount))
        .route("/projects/{id}/workflows", get(project_workflows_page))
        .route(
            "/projects/{id}/workflows/{wf_id}",
            get(project_workflow_detail),
        )
        .route(
            "/projects/{id}/workflows/{wf_id}/trigger",
            post(project_workflow_trigger),
        )
        .route(
            "/projects/{id}/workflows/{wf_id}/runs/{run_id}",
            get(project_workflow_run_detail),
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
async fn index(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<IndexParams>,
) -> Response {
    if extract_user_id(&session).await.is_some() {
        return Redirect::to("/dashboard").into_response();
    }
    LoginTemplate {
        error: params.error,
        dev_auth: state.login == crate::auth::config::LoginMethod::Dev,
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

    let sub = AuthSubject::User(user_id);
    let mcp_creds = state
        .app
        .mcp_creds()
        .list_all_for_user(&sub, user.id)
        .await
        .unwrap_or_default();

    let user_name = user.name.clone().unwrap_or_else(|| user.github_id.clone());

    DashboardTemplate {
        user_name,
        mcp_creds: mcp_creds.iter().map(mcp_creds_to_view).collect(),
    }
    .into_response()
}

#[instrument(name = "web.mcp_creds_list", skip_all)]
async fn mcp_creds_list(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let mcp_creds = state
        .app
        .mcp_creds()
        .list_all_for_user(&sub, user_id)
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
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let entries = state.app.audit().list_recent(50).await;

    match entries {
        Ok(entries) => {
            let mut views = Vec::with_capacity(entries.len());
            for entry in entries {
                let acting_user = match entry.acting_user_id {
                    Some(id) => Some(lookup_user_label(&state.app, id).await),
                    None => None,
                };
                let project = match entry.project_id() {
                    Some(id) => Some(lookup_project_label(&sub, &state.app, id).await),
                    None => None,
                };
                let acting_agent = match entry.acting_agent_id {
                    Some(id) => Some(lookup_agent_label(&sub, &state.app, id).await),
                    None => None,
                };
                let on_behalf_of = match entry.on_behalf_of_user_id {
                    Some(id) => Some(lookup_user_label(&state.app, id).await),
                    None => None,
                };
                views.push(AuditEntryView {
                    acting_user,
                    project,
                    acting_agent,
                    on_behalf_of,
                    entrypoint: entry.entrypoint,
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

#[derive(Debug, Deserialize, Default)]
pub struct LibrarySearchParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default, rename = "projectId")]
    project_id: Option<String>,
    /// Repeated `t=skill&t=note` checkboxes from the form.
    #[serde(default)]
    t: Vec<String>,
}

#[instrument(name = "web.library_page", skip_all)]
async fn library_page(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let projects = state
        .app
        .projects()
        .list_all(&sub)
        .await
        .unwrap_or_default();

    LibraryTemplate {
        projects: projects
            .iter()
            .map(|project| LibraryProjectOption {
                id: project.id.to_string(),
                name: project.name.clone(),
            })
            .collect(),
    }
    .into_response()
}

#[instrument(name = "web.library_search", skip_all)]
async fn library_search(
    State(state): State<AppState>,
    session: Session,
    RepeatedQuery(params): RepeatedQuery<LibrarySearchParams>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let query = params.q.unwrap_or_default();
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return LibrarySearchResultsTemplate {
            query: String::new(),
            hits: vec![],
            error: None,
        }
        .into_response();
    }

    let project_filter: Option<domain::primitives::ProjectId> = match params.project_id.as_deref() {
        Some(s) if !s.is_empty() => match uuid::Uuid::parse_str(s) {
            Ok(uuid) => Some(domain::primitives::ProjectId::from(uuid)),
            Err(_) => {
                return LibrarySearchResultsTemplate {
                    query: trimmed.to_string(),
                    hits: vec![],
                    error: Some("invalid project id".to_string()),
                }
                .into_response();
            }
        },
        _ => None,
    };

    let doc_types: Vec<drua_core::library::DocType> = params
        .t
        .iter()
        .filter_map(|s| match s.as_str() {
            "skill" => Some(drua_core::skill::SKILL_DOC_TYPE),
            "note" => Some(drua_core::note::NOTE_DOC_TYPE),
            "space_file" => Some(drua_core::library::DocType::new("space_file")),
            _ => None,
        })
        .collect();

    let project_ids: Vec<uuid::Uuid> = match project_filter {
        Some(id) => match state.app.projects().find_by_id(&sub, id).await {
            Ok(_) => vec![uuid::Uuid::from(id)],
            Err(e) => {
                tracing::error!(error = %e, "library search project lookup failed");
                return LibrarySearchResultsTemplate {
                    query: trimmed.to_string(),
                    hits: vec![],
                    error: Some(format!("project not found: {e}")),
                }
                .into_response();
            }
        },
        None => Vec::new(),
    };

    let hits = match state
        .app
        .search()
        .search(&sub, &project_ids, trimmed, &doc_types, &[], 50)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "library search failed");
            return LibrarySearchResultsTemplate {
                query: trimmed.to_string(),
                hits: vec![],
                error: Some(format!("search failed: {e}")),
            }
            .into_response();
        }
    };

    // Project-name lookup for display only — auth already enforced above.
    let project_lookup: std::collections::HashMap<uuid::Uuid, String> = state
        .app
        .projects()
        .list_all(&sub)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|project| (uuid::Uuid::from(project.id), project.name))
        .collect();

    let views: Vec<LibraryHitView> = hits
        .into_iter()
        .map(|h| {
            let project_id = h.fields.scope_id;
            // Detail URLs only make sense for project-scoped entities;
            // space-scoped hits get `None` (linking to
            // `/projects/{space_uuid}/...` would 404).
            let path_starts_under_space = h
                .fields
                .path
                .as_deref()
                .is_some_and(|p| p.starts_with("spaces/"));
            let (type_label, type_class, detail_url) = match h.fields.doc_type.as_str() {
                "skill" => (
                    "Skill".to_string(),
                    "skill".to_string(),
                    if path_starts_under_space {
                        None
                    } else {
                        project_id.map(|p| format!("/projects/{}/skills/{}", p, h.fields.doc_id))
                    },
                ),
                "workflow" => (
                    "Workflow".to_string(),
                    "workflow".to_string(),
                    project_id.map(|p| format!("/projects/{}/workflows/{}", p, h.fields.doc_id)),
                ),
                "note" => ("Note".to_string(), "note".to_string(), None),
                "space_file" => ("Space File".to_string(), "space_file".to_string(), None),
                other => (other.to_string(), other.to_string(), None),
            };
            // Two flavours of space-scoped hits surface here:
            //   1. raw space files     (doc_type = "space_file")
            //   2. skills + notes whose path is under `spaces/<slug>/...`
            //      — `scope_id` here is a `SpaceId`, not a `ProjectId`,
            //      so falling into the project branch gives a bare UUID.
            let path_under_space = h
                .fields
                .path
                .as_deref()
                .is_some_and(|p| p.starts_with("spaces/"));
            let is_space_file = h.fields.doc_type.as_str() == "space_file";
            let is_space_scoped = is_space_file || path_under_space;
            let scope_label = if is_space_scoped {
                match (&h.fields.scope_slug, is_space_file, &h.fields.path) {
                    // Raw space file — append the relative path so the
                    // user sees `space: slug/path/inside/space.md`.
                    (Some(slug), true, Some(path)) => format!("space: {slug}/{path}"),
                    (Some(slug), _, _) => format!("space: {slug}"),
                    _ => "space".to_string(),
                }
            } else {
                match project_id {
                    None => "global".to_string(),
                    Some(pid) => {
                        let name = project_lookup
                            .get(&pid)
                            .cloned()
                            .unwrap_or_else(|| pid.to_string());
                        format!("project: {name}")
                    }
                }
            };
            LibraryHitView {
                id: h.fields.doc_id.to_string(),
                scope_label,
                type_label,
                type_class,
                title: h.fields.name,
                snippet: snippet(&h.fields.content, 240),
                score: format!("{:.0}%", h.score * 100.0),
                tags: Vec::new(),
                detail_url,
            }
        })
        .collect();

    LibrarySearchResultsTemplate {
        query: trimmed.to_string(),
        hits: views,
        error: None,
    }
    .into_response()
}

fn snippet(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    let mut out = String::with_capacity(max_chars + 1);
    for ch in trimmed.chars().take(max_chars) {
        out.push(ch);
    }
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

async fn lookup_user_label(app: &drua_core::App, user_id: drua_core::primitives::UserId) -> String {
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
    sub: &AuthSubject,
    app: &drua_core::App,
    agent_id: drua_core::primitives::AgentId,
) -> String {
    app.agents()
        .find_by_id(sub, agent_id)
        .await
        .map(|a| a.name.clone())
        .unwrap_or_else(|_| agent_id.to_string())
}

async fn lookup_project_label(
    sub: &AuthSubject,
    app: &drua_core::App,
    project_id: drua_core::primitives::ProjectId,
) -> String {
    app.projects()
        .find_by_id(sub, project_id)
        .await
        .map(|project| project.name.clone())
        .unwrap_or_else(|_| project_id.to_string())
}

#[instrument(name = "web.code_assistant_dashboard", skip_all)]
async fn code_assistant_dashboard(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let ca = match state.app.code_assistant() {
        Some(ca) => ca,
        None => return CodeAssistantDisabledTemplate {}.into_response(),
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

fn project_to_view(project: &domain::project::Project) -> ProjectView {
    ProjectView {
        id: project.id.to_string(),
        name: project.name.clone(),
        description: project.description.clone().unwrap_or_default(),
        created_at: project
            .created_at()
            .format("%Y-%m-%d %H:%M UTC")
            .to_string(),
        model_chain_override_yaml: project.model_chain_override.as_ref().map(render_chain_yaml),
    }
}

/// Read-only YAML rendering for the override block on the project /
/// agent detail templates. The form posts directly to GraphQL.
fn render_chain_yaml(chain: &drua_core::ModelChain) -> String {
    serde_yaml::to_string(chain).unwrap_or_default()
}

#[instrument(name = "web.projects_page", skip_all)]
async fn projects_page(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let projects = state
        .app
        .projects()
        .list_all(&sub)
        .await
        .unwrap_or_default();

    ProjectHubTemplate {
        projects: projects.iter().map(project_to_view).collect(),
        selected_project: None,
        selected_project_id: String::new(),
        lead_agent: None,
        agents: vec![],
        selected_agent_id: String::new(),
        selected_agent: None,
        selected_agent_model: None,
    }
    .into_response()
}

#[instrument(name = "web.project_sidebar_list", skip_all)]
async fn project_sidebar_list(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    let sub = AuthSubject::User(user_id);

    match state.app.projects().list_all(&sub).await {
        Ok(projects) => ProjectSidebarListTemplate {
            projects: projects.iter().map(project_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list projects for sidebar");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "web.project_new", skip_all)]
async fn project_new(session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    ProjectNewTemplate {}.into_response()
}

#[instrument(name = "web.project_detail", skip_all)]
async fn project_detail(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    ProjectDetailTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
    }
    .into_response()
}

fn secret_to_view(s: &domain::project_secret::ProjectSecret) -> ProjectSecretView {
    ProjectSecretView {
        id: s.id.to_string(),
        project_id: s.project_id.to_string(),
        name: s.name.clone(),
        secret_type: s.secret_type.to_string(),
        updated_at: s.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

#[instrument(name = "web.project_secrets_list", skip_all)]
async fn project_secrets_list(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    match state
        .app
        .project_secrets()
        .list_by_project(&sub, project_id)
        .await
    {
        Ok(secrets) => ProjectSecretsListTemplate {
            secrets: secrets.iter().map(secret_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list project secrets");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn skill_to_view(s: &domain::skill::Skill) -> SkillView {
    SkillView {
        id: s.id.to_string(),
        project_id: s.project_id.map(|id| id.to_string()).unwrap_or_default(),
        name: s.name.clone(),
        description: s.description.clone(),
        body: s.body.clone(),
        created_at: s.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        is_global: s.project_id.is_none(),
    }
}

#[instrument(name = "web.project_skills_page", skip_all)]
async fn project_skills_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    // UI shows the same set the agent sees in `<available_skills>`.
    let scoped_skills = state
        .app
        .skills()
        .list_for_scope(&sub, project_id)
        .await
        .unwrap_or_default();

    let skills: Vec<ScopedSkillView> = scoped_skills
        .into_iter()
        .map(|s| scoped_skill_to_view(&project, s))
        .collect();

    ProjectSkillsPageTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        skills,
        library_repo_url: state.library_repo_url.clone(),
    }
    .into_response()
}

fn scoped_skill_to_view(
    project: &domain::project::Project,
    s: domain::skill::ScopedSkill,
) -> ScopedSkillView {
    use domain::skill::SkillSource;
    let (scope_label, scope_class, detail_url) = match s.source {
        SkillSource::Project {
            skill_id,
            project_id,
        } => (
            "project".to_string(),
            "project".to_string(),
            Some(format!("/projects/{project_id}/skills/{skill_id}")),
        ),
        SkillSource::Global { skill_id: _ } => ("global".to_string(), "global".to_string(), None),
        SkillSource::Space {
            skill_id: _,
            space_id: _,
            space_slug,
        } => (
            format!("space: {space_slug}"),
            "space".to_string(),
            Some(format!("/projects/{}/spaces", project.id)),
        ),
        SkillSource::Sandbox {
            sandbox_id,
            sandbox_name,
        } => (
            format!("sandbox: {sandbox_name}"),
            "sandbox".to_string(),
            Some(format!("/projects/{}/sandboxes/{sandbox_id}", project.id)),
        ),
    };
    ScopedSkillView {
        name: s.name,
        description: s.description,
        scope_label,
        scope_class,
        detail_url,
    }
}

// ── Spaces ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SpacesPageQuery {
    flash: Option<String>,
}

#[instrument(name = "web.project_spaces_page", skip_all)]
async fn project_spaces_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Query(query): Query<SpacesPageQuery>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(p) => p,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    let all_spaces = state.app.spaces().list_all(&sub).await.unwrap_or_default();
    let mounted = state
        .app
        .projects()
        .list_mounted_spaces(&sub, project_id)
        .await
        .unwrap_or_default();
    let mounted_ids: std::collections::HashSet<_> = mounted.iter().map(|s| s.id).collect();

    let mut spaces: Vec<SpaceRowView> = all_spaces
        .iter()
        .map(|s| SpaceRowView {
            id: s.id.to_string(),
            slug: s.slug.clone(),
            description: s.description.clone(),
            mounted: mounted_ids.contains(&s.id),
        })
        .collect();
    // Mounted first, then alphabetical by slug.
    spaces.sort_by(|a, b| match (a.mounted, b.mounted) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.slug.cmp(&b.slug),
    });

    ProjectSpacesPageTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        spaces,
        flash: query.flash,
    }
    .into_response()
}

#[derive(Debug, Deserialize)]
struct SpaceMountForm {
    slug: String,
}

#[instrument(name = "web.project_space_mount", skip_all)]
async fn project_space_mount(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<SpaceMountForm>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);
    let project_id = domain::primitives::ProjectId::from(id);

    let flash = match state
        .app
        .projects()
        .mount_space(&sub, project_id, &form.slug)
        .await
    {
        Ok(space) => format!("Mounted space:{}", space.slug),
        Err(e) => format!("Mount failed: {e}"),
    };
    redirect_to_spaces(project_id, &flash)
}

#[derive(Debug, Deserialize)]
struct SpaceUnmountForm {
    space_id: uuid::Uuid,
}

#[instrument(name = "web.project_space_unmount", skip_all)]
async fn project_space_unmount(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Form(form): Form<SpaceUnmountForm>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);
    let project_id = domain::primitives::ProjectId::from(id);
    let space_id = SpaceId::from(form.space_id);

    let flash = match state
        .app
        .projects()
        .unmount_space(&sub, project_id, space_id)
        .await
    {
        Ok(()) => "Space unmounted.".to_string(),
        Err(e) => format!("Unmount failed: {e}"),
    };
    redirect_to_spaces(project_id, &flash)
}

fn redirect_to_spaces(project_id: domain::primitives::ProjectId, flash: &str) -> Response {
    // Minimal query-string escaping: ampersands and percent break the
    // flash split, spaces survive the round-trip via `+`.
    let encoded: String = flash
        .chars()
        .map(|c| match c {
            ' ' => "+".to_string(),
            '&' => "%26".to_string(),
            '%' => "%25".to_string(),
            '+' => "%2B".to_string(),
            '#' => "%23".to_string(),
            _ => c.to_string(),
        })
        .collect();
    Redirect::to(&format!("/projects/{project_id}/spaces?flash={encoded}")).into_response()
}

#[instrument(name = "web.project_skill_detail", skip_all)]
async fn project_skill_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, skill_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let skill_id = SkillId::from(skill_id);
    let skill = match state
        .app
        .skills()
        .find_by_id(&sub, skill_id, project_id)
        .await
    {
        Ok(s) => s,
        Err(_) => return Redirect::to(&format!("/projects/{id}/skills")).into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    ProjectSkillDetailTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        skill: skill_to_view(&skill),
    }
    .into_response()
}

fn sandbox_to_view(s: &domain::sandbox::Sandbox) -> SandboxView {
    let (mode_label, repo_url, branch) = match &s.mode {
        SandboxMode::Scratch => ("Scratch".to_string(), None, None),
        SandboxMode::Repo { repo_url, branch } => {
            ("Repo".to_string(), Some(repo_url.clone()), branch.clone())
        }
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
            description: sk.description.clone(),
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
        project_id: s.project_id.to_string(),
        name: s.name.clone(),
        state: s.state.to_string(),
        last_error: s.last_error.clone(),
        mode_label,
        repo_url,
        branch,
        cpu: s.specs.cpu.clone(),
        memory: s.specs.memory.clone(),
        disk_size: s.specs.disk_size.clone(),
        created_at: s.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        exports_summary,
        exported_system_prompt,
        exported_skills,
    }
}

/// `lead_agent` is `None` if no ProjectLead exists (shouldn't happen
/// after project create, but handled gracefully).
async fn project_sidebar_context(
    sub: &AuthSubject,
    state: &AppState,
    project_id: domain::primitives::ProjectId,
) -> (Option<AgentView>, Vec<AgentView>) {
    let agents = state
        .app
        .agents()
        .list_for_project(sub, project_id)
        .await
        .unwrap_or_default();

    let mut lead: Option<AgentView> = None;
    let mut others: Vec<AgentView> = Vec::with_capacity(agents.len());
    for a in agents.iter() {
        let view = AgentView {
            id: a.id.to_string(),
            name: a.name.clone(),
        };
        if a.agent_role == domain::agent::AgentRole::ProjectLead && lead.is_none() {
            lead = Some(view);
        } else {
            others.push(view);
        }
    }
    (lead, others)
}

#[instrument(name = "web.project_sandboxes_page", skip_all)]
async fn project_sandboxes_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    let sandboxes = state
        .app
        .sandboxes()
        .list_for_project(&sub, project_id)
        .await
        .unwrap_or_default();

    ProjectSandboxesPageTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        sandboxes: sandboxes.iter().map(sandbox_to_view).collect(),
    }
    .into_response()
}

#[instrument(name = "web.project_sandbox_new", skip_all)]
async fn project_sandbox_new(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    ProjectSandboxNewTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
    }
    .into_response()
}

#[instrument(name = "web.project_sandbox_detail", skip_all)]
async fn project_sandbox_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, sandbox_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let sandbox_id = SandboxId::from(sandbox_id);
    let sandbox = match state.app.sandboxes().find_by_id(&sub, sandbox_id).await {
        Ok(s) => s,
        Err(_) => return Redirect::to(&format!("/projects/{id}/sandboxes")).into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    ProjectSandboxDetailTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        sandbox: sandbox_to_view(&sandbox),
    }
    .into_response()
}

#[instrument(name = "web.project_agent_new", skip_all)]
async fn project_agent_new(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    let sandbox_options: Vec<SandboxOptionView> = state
        .app
        .sandboxes()
        .list_for_project(&sub, project_id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|s| SandboxOptionView {
            id: s.id.to_string(),
            name: s.name.clone(),
            state: s.state.to_string(),
        })
        .collect();

    ProjectAgentNewTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        sandbox_options,
    }
    .into_response()
}

fn role_label(role: domain::agent::AgentRole) -> &'static str {
    match role {
        domain::agent::AgentRole::ProjectLead => "Project Lead",
        domain::agent::AgentRole::Agent => "Agent",
    }
}

async fn attached_sandbox_view(
    sub: &AuthSubject,
    state: &AppState,
    attached: Option<(SandboxId, SandboxAgentMode)>,
) -> Option<AttachedSandboxView> {
    let (sbx_id, mode) = attached?;
    let sandbox = state.app.sandboxes().find_by_id(sub, sbx_id).await.ok()?;
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

#[instrument(name = "web.project_agent_detail", skip_all)]
async fn project_agent_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, agent_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Query(query): Query<AgentDetailQuery>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let agent_id = AgentId::from(agent_id);
    let agent = match state.app.agents().find_by_id(&sub, agent_id).await {
        Ok(a) => a,
        Err(_) => return Redirect::to(&format!("/projects/{id}")).into_response(),
    };

    let (lead_agent, agents) = project_sidebar_context(&sub, &state, project_id).await;

    let attached_view = attached_sandbox_view(&sub, &state, agent.attached_sandbox).await;

    // Dropdown options are only needed when no sandbox is currently attached.
    let sandbox_options: Vec<SandboxOptionView> = if attached_view.is_some() {
        Vec::new()
    } else {
        state
            .app
            .sandboxes()
            .list_for_project(&sub, project_id)
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

    ProjectAgentDetailTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents,
        agent: AgentDetailView {
            id: agent.id.to_string(),
            project_id: agent.project_id.to_string(),
            name: agent.name.clone(),
            role: role_label(agent.agent_role).to_string(),
            is_lead: matches!(agent.agent_role, domain::agent::AgentRole::ProjectLead),
            is_workflow_agent: agent.is_workflow_agent(),
            attached_sandbox: attached_view,
            model_chain_override_yaml: agent.model_chain_override.as_ref().map(render_chain_yaml),
        },
        sandbox_options,
        error: query.error,
    }
    .into_response()
}

#[instrument(name = "web.project_agent_history", skip_all)]
async fn project_agent_history(
    State(state): State<AppState>,
    session: Session,
    Path((id, agent_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let agent_id = AgentId::from(agent_id);
    let agent = match state.app.agents().find_by_id(&sub, agent_id).await {
        Ok(a) => a,
        Err(_) => return Redirect::to(&format!("/projects/{id}")).into_response(),
    };

    // Last 200 messages — long enough for a typical workflow run, short
    // enough to keep the page snappy.
    let history = state
        .app
        .agents()
        .chat_history(&sub, agent_id, 200)
        .await
        .unwrap_or_default();

    let (lead_agent, agents) = project_sidebar_context(&sub, &state, project_id).await;
    let attached_view = attached_sandbox_view(&sub, &state, agent.attached_sandbox).await;
    let workflow_run = match (agent.workflow_id, agent.workflow_run_id) {
        (Some(wf), Some(run)) => Some((wf.to_string(), run.to_string())),
        _ => None,
    };

    let messages: Vec<ChatHistoryMessageView> = history
        .into_iter()
        .map(|m| ChatHistoryMessageView {
            sequence: m.sequence,
            role: match m.role {
                domain::agent::session::history::ChatHistoryRole::User => "user".to_string(),
                domain::agent::session::history::ChatHistoryRole::Assistant => {
                    "assistant".to_string()
                }
            },
            blocks: m
                .blocks
                .into_iter()
                .map(|b| match b {
                    domain::agent::session::history::ChatHistoryBlock::Text { text } => {
                        ChatHistoryBlockView {
                            kind: "text".to_string(),
                            text: Some(text),
                            tool_name: None,
                            tool_input: None,
                            is_error: None,
                        }
                    }
                    domain::agent::session::history::ChatHistoryBlock::Thinking { text } => {
                        ChatHistoryBlockView {
                            kind: "thinking".to_string(),
                            text: Some(text),
                            tool_name: None,
                            tool_input: None,
                            is_error: None,
                        }
                    }
                    domain::agent::session::history::ChatHistoryBlock::ToolUse { name, input } => {
                        ChatHistoryBlockView {
                            kind: "tool_use".to_string(),
                            text: None,
                            tool_name: Some(name),
                            tool_input: Some(
                                serde_json::to_string_pretty(&input)
                                    .unwrap_or_else(|_| input.to_string()),
                            ),
                            is_error: None,
                        }
                    }
                    domain::agent::session::history::ChatHistoryBlock::ToolResult {
                        content,
                        is_error,
                        ..
                    } => ChatHistoryBlockView {
                        kind: "tool_result".to_string(),
                        text: Some(content),
                        tool_name: None,
                        tool_input: None,
                        is_error: Some(is_error),
                    },
                    domain::agent::session::history::ChatHistoryBlock::SandboxNotification {
                        text,
                        ..
                    } => ChatHistoryBlockView {
                        kind: "sandbox_notification".to_string(),
                        text: Some(text),
                        tool_name: None,
                        tool_input: None,
                        is_error: None,
                    },
                })
                .collect(),
        })
        .collect();

    ProjectAgentHistoryTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents,
        agent: AgentDetailView {
            id: agent.id.to_string(),
            project_id: agent.project_id.to_string(),
            name: agent.name.clone(),
            role: role_label(agent.agent_role).to_string(),
            is_lead: matches!(agent.agent_role, domain::agent::AgentRole::ProjectLead),
            is_workflow_agent: agent.is_workflow_agent(),
            attached_sandbox: attached_view,
            model_chain_override_yaml: agent.model_chain_override.as_ref().map(render_chain_yaml),
        },
        messages,
        workflow_run,
    }
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct ChatQuery {
    agent: Option<uuid::Uuid>,
}

#[instrument(name = "web.project_chat", skip_all)]
async fn project_chat(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
    Query(query): Query<ChatQuery>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let all_projects = state
        .app
        .projects()
        .list_all(&sub)
        .await
        .unwrap_or_default();

    let agents = state
        .app
        .agents()
        .list_for_project(&sub, project_id)
        .await
        .unwrap_or_default();

    let selected_agent = match query.agent {
        Some(agent_uuid) => {
            let target = AgentId::from(agent_uuid);
            agents.iter().find(|a| a.id == target)
        }
        None => agents
            .iter()
            .find(|a| a.agent_role == domain::agent::AgentRole::ProjectLead),
    };

    let selected_agent_view = selected_agent.map(|a| AgentView {
        id: a.id.to_string(),
        name: a.name.clone(),
    });
    let selected_agent_model = match selected_agent {
        Some(a) => state
            .app
            .agents()
            .find_session(&sub, a.id)
            .await
            .ok()
            .map(|s| s.model().to_owned()),
        None => None,
    };
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
        if a.agent_role == domain::agent::AgentRole::ProjectLead && lead_agent.is_none() {
            lead_agent = Some(view);
        } else {
            agent_views.push(view);
        }
    }

    ProjectHubTemplate {
        projects: all_projects.iter().map(project_to_view).collect(),
        selected_project_id: project_id.to_string(),
        selected_project: Some(project_to_view(&project)),
        lead_agent,
        agents: agent_views,
        selected_agent_id,
        selected_agent: selected_agent_view,
        selected_agent_model,
    }
    .into_response()
}

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/agents/{id}/secrets", get(api_agent_secrets))
        .route("/tunnel/ws", get(crate::tunnel::tunnel_ws_handler))
}

/// Internal endpoint: returns secret values for an agent's project.
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
    let (project_id, jwt_agent_id) = match &auth {
        AuthSubject::Agent(project_id, agent_id, _) => (*project_id, *agent_id),
        _ => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };

    // The path agent ID must match the one embedded in the JWT
    let path_agent_id = AgentId::from(id);
    if path_agent_id != jwt_agent_id {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let secrets = match state
        .app
        .project_secrets()
        .list_decrypted(&auth, project_id)
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

/// Percent-encode anything outside the RFC 3986 §2.3 unreserved set.
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

fn workflow_definition_to_view_for_list(
    d: &domain::workflow::WorkflowDefinition,
) -> WorkflowDefinitionView {
    let (trigger_type, trigger_provider, secret) = match &d.trigger {
        WorkflowTrigger::Manual => ("manual".to_string(), None, None),
        WorkflowTrigger::Webhook { provider, secret } => (
            "webhook".to_string(),
            provider.clone(),
            Some(secret.clone()),
        ),
        WorkflowTrigger::Cron { schedule, .. } => (format!("cron ({schedule})"), None, None),
    };
    // The list view never surfaces the secret — only the detail page does.
    let _ = secret;
    WorkflowDefinitionView {
        id: d.id.to_string(),
        name: d.name.clone(),
        description: d.description.clone(),
        trigger_type,
        trigger_provider,
        webhook_secret: None,
        webhook_url: None,
        steps: d.steps.iter().map(workflow_step_to_view).collect(),
        sandboxes: d.sandboxes.iter().map(workflow_sandbox_to_view).collect(),
        created_at: d.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

fn workflow_definition_to_view_for_detail(
    d: &domain::workflow::WorkflowDefinition,
    public_host: Option<&str>,
) -> WorkflowDefinitionView {
    let (trigger_type, trigger_provider, secret) = match &d.trigger {
        WorkflowTrigger::Manual => ("manual".to_string(), None, None),
        WorkflowTrigger::Webhook { provider, secret } => (
            "webhook".to_string(),
            provider.clone(),
            Some(secret.clone()),
        ),
        WorkflowTrigger::Cron { schedule, timezone } => {
            let label = match timezone {
                Some(tz) => format!("cron ({schedule} {tz})"),
                None => format!("cron ({schedule})"),
            };
            (label, None, None)
        }
    };
    let webhook_url = secret.as_ref().map(|_| match public_host {
        Some(host) => format!("{host}/webhooks/{}", d.id),
        None => format!("/webhooks/{}", d.id),
    });
    WorkflowDefinitionView {
        id: d.id.to_string(),
        name: d.name.clone(),
        description: d.description.clone(),
        trigger_type,
        trigger_provider,
        webhook_secret: secret,
        webhook_url,
        steps: d.steps.iter().map(workflow_step_to_view).collect(),
        sandboxes: d.sandboxes.iter().map(workflow_sandbox_to_view).collect(),
        created_at: d.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

fn workflow_sandbox_to_view(d: &domain::workflow::WorkflowSandboxDecl) -> WorkflowSandboxView {
    use domain::workflow::WorkflowSandboxDecl;
    match d {
        WorkflowSandboxDecl::Preexisting { name } => WorkflowSandboxView {
            name: name.clone(),
            kind: "preexisting".to_string(),
            repo_url: None,
            branch: None,
        },
        WorkflowSandboxDecl::Provisioned { name, mode, .. } => {
            let (kind, repo_url, branch) = match mode {
                SandboxMode::Scratch => ("scratch".to_string(), None, None),
                SandboxMode::Repo { repo_url, branch } => {
                    ("repo".to_string(), Some(repo_url.clone()), branch.clone())
                }
            };
            WorkflowSandboxView {
                name: name.clone(),
                kind,
                repo_url,
                branch,
            }
        }
    }
}

fn workflow_step_to_view(s: &domain::workflow::WorkflowStepDef) -> WorkflowStepView {
    match s {
        domain::workflow::WorkflowStepDef::AgentStep {
            name,
            skill,
            sandbox,
            timeout_seconds,
            ..
        } => WorkflowStepView {
            name: name.clone(),
            step_type: "agent_step".to_string(),
            skill: skill.clone(),
            sandbox: sandbox.clone(),
            timeout_seconds: *timeout_seconds,
        },
    }
}

fn workflow_run_state_str(state: domain::workflow::WorkflowRunState) -> &'static str {
    match state {
        domain::workflow::WorkflowRunState::Pending => "pending",
        domain::workflow::WorkflowRunState::Running => "running",
        domain::workflow::WorkflowRunState::Succeeded => "succeeded",
        domain::workflow::WorkflowRunState::Failed => "failed",
    }
}

fn workflow_run_to_view(r: &domain::workflow::WorkflowRun) -> WorkflowRunView {
    let output_summary = r
        .step_results
        .last()
        .and_then(|sr| {
            if let Some(err) = &sr.error {
                Some(format!("ERROR: {err}"))
            } else {
                sr.output.as_ref().map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
            }
        })
        .map(|s| {
            const MAX: usize = 100;
            let oneline = s.replace('\n', " ");
            if oneline.chars().count() > MAX {
                format!("{}…", oneline.chars().take(MAX).collect::<String>())
            } else {
                oneline
            }
        })
        .unwrap_or_else(|| "—".to_string());

    WorkflowRunView {
        id: r.id.to_string(),
        definition_id: r.definition_id.to_string(),
        state: workflow_run_state_str(r.state).to_string(),
        started_at: r.started_at().format("%Y-%m-%d %H:%M:%SZ").to_string(),
        completed_at: r
            .completed_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%SZ").to_string()),
        output_summary,
    }
}

fn step_result_to_view(sr: &domain::workflow::StepResult) -> StepResultView {
    let state = if sr.error.is_some() {
        "failed"
    } else if sr.completed_at.is_some() {
        "succeeded"
    } else {
        "running"
    };
    StepResultView {
        name: sr.name.clone(),
        state: state.to_string(),
        output: sr.output.as_ref().map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        }),
        error: sr.error.clone(),
        completed_at: sr
            .completed_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%SZ").to_string()),
    }
}

#[instrument(name = "web.project_workflows_page", skip_all)]
async fn project_workflows_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    let workflows = state
        .app
        .workflows()
        .list_for_project(&sub, project_id)
        .await
        .unwrap_or_default();

    ProjectWorkflowsPageTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        workflows: workflows
            .iter()
            .map(workflow_definition_to_view_for_list)
            .collect(),
    }
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
struct WorkflowDetailQuery {
    flash: Option<String>,
}

#[instrument(name = "web.project_workflow_detail", skip_all)]
async fn project_workflow_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, wf_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Query(query): Query<WorkflowDetailQuery>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let workflow_id = WorkflowDefinitionId::from(wf_id);
    let workflow = match state.app.workflows().find_by_id(&sub, workflow_id).await {
        Ok(w) if w.project_id == project_id => w,
        _ => {
            return Redirect::to(&format!("/projects/{id}/workflows")).into_response();
        }
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    let recent_runs = state
        .app
        .workflows()
        .list_runs(&sub, workflow_id)
        .await
        .unwrap_or_default();

    ProjectWorkflowDetailTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        workflow: workflow_definition_to_view_for_detail(&workflow, None),
        recent_runs: recent_runs.iter().map(workflow_run_to_view).collect(),
        flash: query.flash,
    }
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
struct WorkflowTriggerForm {
    payload: Option<String>,
}

#[instrument(name = "web.project_workflow_trigger", skip_all)]
async fn project_workflow_trigger(
    State(state): State<AppState>,
    session: Session,
    Path((id, wf_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Form(form): Form<WorkflowTriggerForm>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let workflow_id = WorkflowDefinitionId::from(wf_id);

    // Bad JSON bounces back with a flash rather than 400 so the
    // operator can correct in place.
    let payload = match form
        .payload
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("invalid JSON payload: {e}");
                return Redirect::to(&format!(
                    "/projects/{id}/workflows/{wf_id}?flash={}",
                    encode_query_value(&msg)
                ))
                .into_response();
            }
        },
        None => serde_json::Value::Null,
    };

    match state
        .app
        .workflows()
        .trigger_run(&sub, workflow_id, payload)
        .await
    {
        Ok(run) => Redirect::to(&format!("/projects/{id}/workflows/{wf_id}/runs/{}", run.id))
            .into_response(),
        Err(e) => {
            let msg = format!("failed to trigger run: {e}");
            Redirect::to(&format!(
                "/projects/{id}/workflows/{wf_id}?flash={}",
                encode_query_value(&msg)
            ))
            .into_response()
        }
    }
}

#[instrument(name = "web.project_workflow_run_detail", skip_all)]
async fn project_workflow_run_detail(
    State(state): State<AppState>,
    session: Session,
    Path((id, wf_id, run_id)): Path<(uuid::Uuid, uuid::Uuid, uuid::Uuid)>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };
    let sub = AuthSubject::User(user_id);

    let project_id = domain::primitives::ProjectId::from(id);
    let project = match state.app.projects().find_by_id(&sub, project_id).await {
        Ok(project) => project,
        Err(_) => return Redirect::to("/projects").into_response(),
    };

    let workflow_id = WorkflowDefinitionId::from(wf_id);
    let workflow = match state.app.workflows().find_by_id(&sub, workflow_id).await {
        Ok(w) if w.project_id == project_id => w,
        _ => return Redirect::to(&format!("/projects/{id}/workflows")).into_response(),
    };

    let run_id = WorkflowRunId::from(run_id);
    let run = match state.app.workflows().find_run_by_id(&sub, run_id).await {
        Ok(r) if r.project_id == project_id && r.definition_id == workflow_id => r,
        _ => {
            return Redirect::to(&format!("/projects/{id}/workflows/{wf_id}")).into_response();
        }
    };

    let (lead_agent, agent_views) = project_sidebar_context(&sub, &state, project_id).await;

    let run_agents = state
        .app
        .agents()
        .list_for_workflow_run(&sub, project_id, run_id)
        .await
        .unwrap_or_default();

    let steps = run
        .step_results
        .iter()
        .map(step_result_to_view)
        .collect::<Vec<_>>();

    ProjectWorkflowRunDetailTemplate {
        project: project_to_view(&project),
        lead_agent,
        agents: agent_views,
        workflow_id: workflow.id.to_string(),
        workflow_name: workflow.name.clone(),
        run: workflow_run_to_view(&run),
        steps,
        run_agents: run_agents
            .iter()
            .map(|a| AgentView {
                id: a.id.to_string(),
                name: a.name.clone(),
            })
            .collect(),
    }
    .into_response()
}
