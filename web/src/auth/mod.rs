pub mod config;
pub mod error;
mod oauth;
pub mod sa_token;
pub mod session_store;

use axum::{extract::Request, middleware::Next, response::Response, routing::get, Router};
use tower_sessions::Session;
use tracing::instrument;

pub use config::AuthConfig;
pub use error::AuthError;

use galoy_agents_core as domain;

use domain::auth::AuthContext;
use domain::mcp_creds::token::hash_token;
use domain::primitives::UserId;

use crate::AppState;

/// Build the router for GitHub OAuth endpoints.
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/auth/github", get(oauth::github_redirect))
        .route("/auth/github/callback", get(oauth::github_callback))
        .route("/auth/logout", get(logout))
}

/// Axum middleware that resolves [`AuthContext`] and inserts it into request extensions.
///
/// Extracts headers and session synchronously from the request, then performs
/// async lookups, to avoid holding `&Request` across `.await` boundaries.
#[instrument(name = "web.auth.middleware", skip_all)]
pub async fn auth_middleware(mut request: Request, next: Next) -> Response {
    let state = request.extensions().get::<AppState>().cloned();
    let session = request.extensions().get::<Session>().cloned();
    let bearer_token = extract_bearer_token(&request);

    let auth_context = resolve_auth_context(state.as_ref(), session.as_ref(), bearer_token).await;
    request.extensions_mut().insert(auth_context);
    next.run(request).await
}

fn extract_bearer_token(request: &Request) -> Option<String> {
    let header_value = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let raw_token = header_value.strip_prefix("Bearer ")?;
    Some(raw_token.to_string())
}

async fn resolve_auth_context(
    state: Option<&AppState>,
    session: Option<&Session>,
    bearer_token: Option<String>,
) -> AuthContext {
    let state = match state {
        Some(s) => s,
        None => return AuthContext::Anonymous,
    };

    // 1. Check Authorization: Bearer header
    if let Some(raw_token) = bearer_token {
        // 1a. Hash-based lookup (user-created MCP credentials + legacy agent tokens)
        let token_hash = hash_token(&raw_token);
        if let Ok(Some(creds)) = state.app.mcp_creds().find_by_token_hash(&token_hash).await {
            if !creds.is_revoked() {
                match &creds.owner {
                    galoy_agents_core::primitives::McpCredsOwner::User { user_id } => {
                        return AuthContext::ExportedAgent(
                            *user_id,
                            creds.id,
                            creds.scopes.clone(),
                        );
                    }
                    galoy_agents_core::primitives::McpCredsOwner::Agent { agent_id } => {
                        let synthetic_user_id = galoy_agents_core::primitives::UserId::from(
                            uuid::Uuid::from(*agent_id),
                        );
                        return AuthContext::ExportedAgent(
                            synthetic_user_id,
                            creds.id,
                            creds.scopes.clone(),
                        );
                    }
                }
            }
        }

        // 1b. SA token validation (sandbox pods with projected ServiceAccount tokens)
        if sa_token::looks_like_jwt(&raw_token) {
            if let Some(ref validator) = state.sa_token_validator {
                if let Ok(agent_id) = validator.validate(&raw_token).await {
                    if let Ok(agent) = state.app.agents().find_by_id(agent_id).await {
                        return AuthContext::Agent(
                            agent.workspace_id,
                            agent.id,
                            vec!["agent".to_string()],
                        );
                    }
                }
            }
        }
    }

    // 2. Check session cookie
    if let Some(session) = session {
        if let Ok(Some(user_id)) = session.get::<UserId>("user_id").await {
            return AuthContext::User(user_id);
        }
    }

    AuthContext::Anonymous
}

#[instrument(name = "web.auth.logout", skip_all)]
async fn logout(session: Session) -> axum::response::Redirect {
    let _ = session.flush().await;
    axum::response::Redirect::to("/")
}
