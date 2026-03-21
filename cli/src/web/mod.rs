mod templates;

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use sha2::{Digest, Sha256};
use tracing::instrument;

use mcp_gateway::{
    agent::{Agent, Agents},
    primitives::{AgentId, UserId},
    user::Users,
};

use templates::*;

#[derive(Clone)]
pub struct AppState {
    pub users: Users,
    pub agents: Agents,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/dashboard", get(dashboard))
        .route("/dashboard/agents", get(agent_list))
        .route("/agents/create", post(create_agent))
        .route("/agents/{id}/revoke", post(revoke_agent))
        .with_state(Arc::new(state))
}

fn extract_user_id(_headers: &axum::http::HeaderMap) -> Option<UserId> {
    // TODO: Extract user_id from session cookie once OAuth is implemented.
    // For now, this is a placeholder that returns None (not authenticated).
    None
}

fn agent_to_view(agent: &Agent) -> AgentView {
    AgentView {
        id: agent.id.to_string(),
        name: agent.name.clone(),
        scopes: agent.scopes.join(", "),
        created_at: agent.created_at().format("%Y-%m-%d %H:%M UTC").to_string(),
        is_revoked: agent.is_revoked(),
    }
}

fn hash_token(token: &str) -> String {
    let hash = Sha256::digest(token.as_bytes());
    format!("{hash:x}")
}

#[instrument(name = "web.index", skip_all)]
async fn index(State(state): State<Arc<AppState>>) -> Response {
    let _state = state;

    // TODO: Check session for logged-in user, redirect to dashboard if found.
    // For now, always show login page.
    LoginTemplate.into_response()
}

#[instrument(name = "web.dashboard", skip_all)]
async fn dashboard(State(state): State<Arc<AppState>>) -> Response {
    let user_id = match extract_user_id(&axum::http::HeaderMap::new()) {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let user = match state.users.find_by_id(user_id).await {
        Ok(user) => user,
        Err(_) => return Redirect::to("/").into_response(),
    };

    let agents = state
        .agents
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
    scopes: String,
}

#[instrument(name = "web.create_agent", skip_all)]
async fn create_agent(
    State(state): State<Arc<AppState>>,
    Form(form): Form<CreateAgentForm>,
) -> Response {
    let user_id = match extract_user_id(&axum::http::HeaderMap::new()) {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let raw_token = uuid::Uuid::new_v4().to_string();
    let token_hash = hash_token(&raw_token);
    let scopes: Vec<String> = form
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    match state
        .agents
        .create_for_user(user_id, &form.name, token_hash, scopes)
        .await
    {
        Ok(_agent) => AgentCreatedTemplate {
            token: raw_token,
            agent_name: form.name,
        }
        .into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.revoke_agent", skip_all)]
async fn revoke_agent(State(state): State<Arc<AppState>>, Path(id): Path<uuid::Uuid>) -> Response {
    let _user_id = match extract_user_id(&axum::http::HeaderMap::new()) {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let agent_id = AgentId::from(id);
    match state.agents.revoke(agent_id).await {
        Ok(agent) => AgentRowTemplate {
            agent: agent_to_view(&agent),
        }
        .into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[instrument(name = "web.agent_list", skip_all)]
async fn agent_list(State(state): State<Arc<AppState>>) -> Response {
    let user_id = match extract_user_id(&axum::http::HeaderMap::new()) {
        Some(id) => id,
        None => return Redirect::to("/").into_response(),
    };

    let agents = state
        .agents
        .list_all_for_user(user_id)
        .await
        .unwrap_or_default();

    AgentListTemplate {
        agents: agents.iter().map(agent_to_view).collect(),
    }
    .into_response()
}
