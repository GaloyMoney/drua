pub mod config;
pub mod error;
mod oauth;
pub mod session_store;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
    routing::get,
    Router,
};
use tower_sessions::Session;
use tracing::instrument;

pub use config::AuthConfig;
pub use error::AuthError;

use galoy_agents_domain as domain;

use domain::agent::Agents;
use domain::auth::token::hash_token;
use domain::auth::AuthContext;
use domain::primitives::*;
use domain::user::Users;

use self::config::OAuthClient;

/// Shared application state for auth routes and middleware.
#[derive(Clone)]
pub struct AuthAppState {
    pub(crate) users: Users,
    pub(crate) agents: Agents,
    pub(crate) oauth_client: OAuthClient,
}

impl AuthAppState {
    pub fn new(users: Users, agents: Agents, oauth_client: OAuthClient) -> Self {
        Self {
            users,
            agents,
            oauth_client,
        }
    }
}

/// Build the router for GitHub OAuth endpoints.
pub fn auth_router() -> Router<AuthAppState> {
    Router::new()
        .route("/auth/github", get(oauth::github_redirect))
        .route("/auth/github/callback", get(oauth::github_callback))
}

/// Axum middleware that resolves [`AuthContext`] and inserts it into request extensions.
#[instrument(name = "web.auth.middleware", skip_all)]
pub async fn auth_middleware(
    State(state): State<AuthAppState>,
    session: Session,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_context = resolve_auth_context(&state, &session, &request).await;
    request.extensions_mut().insert(auth_context);
    next.run(request).await
}

async fn resolve_auth_context(
    state: &AuthAppState,
    session: &Session,
    request: &Request,
) -> AuthContext {
    // 1. Check Authorization: Bearer header
    if let Some(auth_context) = resolve_bearer_token(state, request).await {
        return auth_context;
    }

    // 2. Check session cookie
    if let Some(auth_context) = resolve_session(session).await {
        return auth_context;
    }

    AuthContext::Anonymous
}

async fn resolve_bearer_token(state: &AuthAppState, request: &Request) -> Option<AuthContext> {
    let header_value = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;

    let raw_token = header_value.strip_prefix("Bearer ")?;
    let token_hash = hash_token(raw_token);

    match state.agents.find_by_token_hash(&token_hash).await {
        Ok(Some(agent)) if !agent.is_revoked() => Some(AuthContext::Agent(agent.id, agent.user_id)),
        _ => None,
    }
}

async fn resolve_session(session: &Session) -> Option<AuthContext> {
    let user_id: UserId = session.get("user_id").await.ok()??;
    Some(AuthContext::User(user_id))
}
