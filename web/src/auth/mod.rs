pub mod config;
pub mod error;
mod oauth;
pub mod sa_token;
pub mod session_store;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tower_sessions::Session;
use tracing::instrument;

pub use config::AuthConfig;
pub use error::AuthError;

use galoy_agents_core as domain;

use domain::auth::AuthSubject;
use domain::mcp_creds::token::{generate_token, hash_token};
use domain::primitives::UserId;

use crate::AppState;

/// Build the router for GitHub OAuth and dev-login endpoints.
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/auth/github", get(oauth::github_redirect))
        .route("/auth/github/callback", get(oauth::github_callback))
        .route("/auth/dev", post(dev_login))
        .route("/auth/cli/token", post(cli_token_exchange))
        .route("/auth/logout", get(logout))
}

/// Axum middleware that resolves [`AuthSubject`] and inserts it into request extensions.
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
    // 1. Check Authorization: Bearer header
    if let Some(header_value) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(raw_token) = header_value.strip_prefix("Bearer ") {
            return Some(raw_token.to_string());
        }
    }

    // 2. Fallback: check ?token= query parameter (for MCP clients that
    //    don't support custom headers in server config)
    request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(|t| {
                t.replace("%2B", "+")
                    .replace("%2F", "/")
                    .replace("%3D", "=")
            })
    })
}

async fn resolve_auth_context(
    state: Option<&AppState>,
    session: Option<&Session>,
    bearer_token: Option<String>,
) -> AuthSubject {
    let state = match state {
        Some(s) => s,
        None => return AuthSubject::Anonymous,
    };

    // 1. Check Authorization: Bearer header
    if let Some(raw_token) = bearer_token {
        // 1a. Hash-based lookup (user-created MCP credentials + legacy agent tokens)
        let token_hash = hash_token(&raw_token);
        if let Ok(Some(creds)) = state.app.mcp_creds().find_by_token_hash(&token_hash).await {
            if !creds.is_revoked() {
                match &creds.owner {
                    galoy_agents_core::primitives::McpCredsOwner::User { user_id } => {
                        return AuthSubject::ExportedAgent(
                            *user_id,
                            creds.id,
                            creds.scopes.clone(),
                        );
                    }
                    galoy_agents_core::primitives::McpCredsOwner::Agent { agent_id } => {
                        let synthetic_user_id = galoy_agents_core::primitives::UserId::from(
                            uuid::Uuid::from(*agent_id),
                        );
                        return AuthSubject::ExportedAgent(
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
                if let Ok(id_str) = validator.validate(&raw_token).await {
                    let agent_id = domain::primitives::AgentId::from(
                        id_str.parse::<uuid::Uuid>().expect("validated as UUID"),
                    );
                    if let Ok(agent) = state.app.agents().find_by_id(agent_id).await {
                        return agent.auth_subject();
                    }
                }
            }
        }
    }

    // 2. Check session cookie
    if let Some(session) = session {
        if let Ok(Some(user_id)) = session.get::<UserId>("user_id").await {
            return AuthSubject::User(user_id);
        }
    }

    AuthSubject::Anonymous
}

#[instrument(name = "web.auth.dev_login", skip_all)]
async fn dev_login(
    State(state): State<AppState>,
    session: Session,
) -> Result<axum::response::Redirect, AuthError> {
    if state.login != crate::auth::config::LoginMethod::Dev {
        return Err(AuthError::OAuth("Dev login is not enabled".into()));
    }

    let dev_github_id = "dev-local";
    let user = match state.app.users().find_by_github_id(dev_github_id).await? {
        Some(user) => user,
        None => {
            state
                .app
                .users()
                .create_from_github_login(
                    dev_github_id.to_string(),
                    Some("dev@localhost".to_string()),
                    Some("Dev User".to_string()),
                )
                .await?
        }
    };

    session.insert("user_id", user.id).await?;
    Ok(axum::response::Redirect::to("/"))
}

#[derive(Serialize)]
struct CliTokenResponse {
    token: String,
    user_id: String,
}

/// Exchange a GitHub access token for a CLI bearer token.
///
/// The CLI obtains a GitHub token via the OAuth Device Flow, then calls this
/// endpoint to convert it into an MCP credential bearer token. The server
/// validates the GitHub token, finds/creates the user, and returns a new
/// bearer token for subsequent API calls.
#[instrument(name = "web.auth.cli_token_exchange", skip_all)]
async fn cli_token_exchange(State(state): State<AppState>, request: Request) -> Response {
    let github_token = match extract_bearer_token(&request) {
        Some(t) => t,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "missing Authorization: Bearer header"})),
            )
                .into_response()
        }
    };

    let github_user =
        match oauth::fetch_github_user(&github_token).await {
            Ok(u) => u,
            Err(e) => return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": format!("GitHub token validation failed: {e}")})),
            )
                .into_response(),
        };

    if !state.github_allowed_teams.is_empty() {
        let teams_ok = match oauth::fetch_github_teams(&github_token).await {
            Ok(teams) => teams.iter().any(|team| {
                let full_slug = format!("{}/{}", team.organization.login, team.slug);
                state
                    .github_allowed_teams
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&full_slug))
            }),
            Err(_) => false,
        };
        if !teams_ok {
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "GitHub account is not in an authorized team"})),
            )
                .into_response();
        }
    }

    let github_id_str = github_user.id.to_string();
    let user = match state.app.users().find_by_github_id(&github_id_str).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            match state
                .app
                .users()
                .create_from_github_login(github_id_str, github_user.email, github_user.name)
                .await
            {
                Ok(user) => user,
                Err(e) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": format!("failed to create user: {e}")})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("user lookup failed: {e}")})),
            )
                .into_response()
        }
    };

    let (raw_token, token_hash) = generate_token();

    match state
        .app
        .mcp_creds()
        .create_for_user(user.id, "drua-cli", token_hash, vec![])
        .await
    {
        Ok(_) => Json(CliTokenResponse {
            token: raw_token,
            user_id: user.id.to_string(),
        })
        .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to create credentials: {e}")})),
        )
            .into_response(),
    }
}

#[instrument(name = "web.auth.logout", skip_all)]
async fn logout(session: Session) -> axum::response::Redirect {
    let _ = session.flush().await;
    axum::response::Redirect::to("/")
}
