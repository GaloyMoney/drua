pub mod config;
pub mod error;
mod oauth;
pub mod session_store;
pub mod token;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower_sessions::Session;
use tracing::instrument;

pub use config::AuthConfig;
pub use error::AuthError;

use crate::agent::Agents;
use crate::primitives::*;
use crate::user::Users;

use self::config::OAuthClient;
use self::token::hash_token;

/// Unified authentication context resolved from session or bearer token.
#[derive(Debug, Clone)]
pub enum AuthContext {
    User(UserId),
    Agent(AgentId, UserId),
    Anonymous,
}

/// Shared application state for auth routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub(crate) users: Users,
    pub(crate) agents: Agents,
    pub(crate) oauth_client: OAuthClient,
}

impl AppState {
    pub fn new(users: Users, agents: Agents, oauth_client: OAuthClient) -> Self {
        Self {
            users,
            agents,
            oauth_client,
        }
    }
}

/// Build the router for GitHub OAuth endpoints.
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/auth/github", get(oauth::github_redirect))
        .route("/auth/github/callback", get(oauth::github_callback))
}

/// Axum middleware that resolves [`AuthContext`] and inserts it into request extensions.
///
/// Returns 401 when credentials are present but invalid. Only falls through to
/// [`AuthContext::Anonymous`] when no credentials are provided at all.
#[instrument(name = "mcp_gateway.auth.middleware", skip_all)]
pub async fn auth_middleware(
    State(state): State<AppState>,
    session: Session,
    mut request: Request,
    next: Next,
) -> Response {
    match resolve_auth_context(&state, &session, &request).await {
        Ok(ctx) => {
            request.extensions_mut().insert(ctx);
            next.run(request).await
        }
        Err(resp) => resp,
    }
}

async fn resolve_auth_context(
    state: &AppState,
    session: &Session,
    request: &Request,
) -> Result<AuthContext, Response> {
    // 1. If Authorization: Bearer header is present, it MUST resolve or fail.
    if let Some(result) = try_resolve_bearer_token(state, request).await {
        return result;
    }

    // 2. If session contains user_id, it MUST resolve or fail.
    if let Some(result) = try_resolve_session(session).await {
        return result;
    }

    // 3. No credentials provided at all.
    Ok(AuthContext::Anonymous)
}

/// Returns `Some` if a Bearer header is present (meaning the caller tried to
/// authenticate with a token). Returns `None` if no Bearer header is present.
async fn try_resolve_bearer_token(
    state: &AppState,
    request: &Request,
) -> Option<Result<AuthContext, Response>> {
    let header_value = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;

    let raw_token = match header_value.strip_prefix("Bearer ") {
        Some(t) => t,
        None => return None,
    };

    let token_hash = hash_token(raw_token);

    let result = match state.agents.find_by_token_hash(&token_hash).await {
        Ok(Some(agent)) if !agent.is_revoked() => Ok(AuthContext::Agent(agent.id, agent.user_id)),
        Ok(Some(_)) => {
            tracing::warn!("bearer token lookup matched a revoked agent");
            Err(unauthorized())
        }
        Ok(None) => {
            tracing::warn!("bearer token lookup found no matching agent");
            Err(unauthorized())
        }
        Err(e) => {
            tracing::error!(error = %e, "bearer token lookup failed");
            Err(internal_error())
        }
    };

    Some(result)
}

/// Returns `Some` if the session contains a user_id key (meaning the caller
/// has an active session). Returns `None` if session is empty / no cookie.
async fn try_resolve_session(session: &Session) -> Option<Result<AuthContext, Response>> {
    match session.get::<UserId>("user_id").await {
        Ok(Some(user_id)) => Some(Ok(AuthContext::User(user_id))),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(error = %e, "session lookup failed");
            Some(Err(internal_error()))
        }
    }
}

fn unauthorized() -> Response {
    (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
}

fn internal_error() -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "Internal server error",
    )
        .into_response()
}
