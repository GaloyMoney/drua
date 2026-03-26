use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use serde::Deserialize;
use tower_sessions::Session;
use tracing::instrument;

use galoy_agents_domain as domain;

use domain::agent::token::generate_token;
use domain::agent::Agent;
use domain::primitives::{AgentId, UserId};

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
        .route("/dashboard/agents", get(agent_list))
        .route("/agents/create", post(create_agent))
        .route("/agents/{id}/revoke", post(revoke_agent))
        .route("/style-agent", get(style_agent_dashboard))
        .route("/style-agent/recent", get(style_agent_recent))
        .route("/style-agent/least-useful", get(style_agent_least_useful))
}

async fn extract_user_id(session: &Session) -> Option<UserId> {
    session.get("user_id").await.ok()?
}

fn agent_to_view(agent: &Agent) -> AgentView {
    AgentView {
        id: agent.id.to_string(),
        name: agent.name.clone(),
        created_at: agent.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        is_revoked: agent.is_revoked(),
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

    let agents = state
        .app
        .agents()
        .list_all_for_user(user.id)
        .await
        .unwrap_or_default();

    let user_name = user.name.clone().unwrap_or_else(|| user.github_id.clone());

    DashboardTemplate {
        user_name,
        agents: agents.iter().map(agent_to_view).collect(),
    }
    .into_response()
}

#[derive(serde::Deserialize)]
pub struct CreateAgentForm {
    name: String,
}

fn build_mcp_json(mcp_endpoint: &str, token: &str) -> String {
    serde_json::json!({
        "galoy-agents": {
            "type": "http",
            "url": mcp_endpoint,
            "headers": {
                "Authorization": format!("Bearer {token}")
            }
        }
    })
    .to_string()
}

#[instrument(name = "web.create_agent", skip_all)]
async fn create_agent(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<CreateAgentForm>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let (raw_token, token_hash) = generate_token();

    match state
        .app
        .agents()
        .create_for_user(user_id, &form.name, token_hash, vec![])
        .await
    {
        Ok(_agent) => {
            let mcp_json = build_mcp_json(&state.mcp_endpoint, &raw_token);
            AgentCreatedTemplate {
                agent_name: form.name,
                mcp_json,
            }
            .into_response()
        }
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.revoke_agent", skip_all)]
async fn revoke_agent(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let agent_id = AgentId::from(id);
    match state.app.agents().revoke(user_id, agent_id).await {
        Ok(agent) => AgentRowTemplate {
            agent: agent_to_view(&agent),
        }
        .into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.agent_list", skip_all)]
async fn agent_list(State(state): State<AppState>, session: Session) -> Response {
    let user_id = match extract_user_id(&session).await {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let agents = state
        .app
        .agents()
        .list_all_for_user(user_id)
        .await
        .unwrap_or_default();

    AgentListTemplate {
        agents: agents.iter().map(agent_to_view).collect(),
    }
    .into_response()
}

#[instrument(name = "web.style_agent_dashboard", skip_all)]
async fn style_agent_dashboard(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }

    let stats = match state.app.style_agent_logs().dashboard_stats().await {
        Ok(stats) => stats,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load style-agent stats");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    StyleAgentTemplate { stats }.into_response()
}

#[instrument(name = "web.style_agent_recent", skip_all)]
async fn style_agent_recent(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let rows = match state.app.style_agent_logs().recent_requests(50).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load recent requests");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    StyleAgentRecentTemplate { rows }.into_response()
}

#[instrument(name = "web.style_agent_least_useful", skip_all)]
async fn style_agent_least_useful(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let rows = match state.app.style_agent_logs().least_useful(50).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load least useful requests");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    StyleAgentRecentTemplate { rows }.into_response()
}
