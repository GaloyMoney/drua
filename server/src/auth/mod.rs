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
    Router,
};
use tower_sessions::session::{Id, Record};
use tower_sessions::session_store::SessionStore;
use tower_sessions::Session;
use tracing::instrument;

pub use config::AuthConfig;
pub use error::AuthError;

use drua_core as domain;

use domain::auth::AuthSubject;
use domain::mcp_creds::token::hash_token;
use domain::primitives::UserId;

use crate::templates::{CliLoginTemplate, CliTokenTemplate};
use crate::AppState;

pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/auth/github", get(oauth::github_redirect))
        .route("/auth/github/callback", get(oauth::github_callback))
        .route("/auth/dev", post(dev_login))
        .route("/auth/logout", get(logout))
        .route("/auth/cli-login", get(cli_login))
}

/// Resolves [`AuthSubject`] and inserts it into request extensions.
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
    if let Some(header_value) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(raw_token) = header_value.strip_prefix("Bearer ") {
            return Some(raw_token.to_string());
        }
    }

    // Fallback for MCP clients that can't set custom headers.
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

    if let Some(raw_token) = bearer_token {
        // Session-ID resolution (CLI tokens from /auth/cli-login).
        // Session IDs are 22-char base64url; MCP tokens are 43-char — the parse
        // naturally rejects non-session tokens with zero ambiguity.
        if let Ok(session_id) = raw_token.parse::<Id>() {
            if let Ok(Some(record)) = state.session_store.load(&session_id).await {
                if let Some(user_id) = record
                    .data
                    .get("user_id")
                    .and_then(|v| serde_json::from_value::<UserId>(v.clone()).ok())
                {
                    return AuthSubject::User(user_id);
                }
            }
        }

        // Hash-based lookup: user-created MCP credentials + legacy agent tokens.
        let token_hash = hash_token(&raw_token);
        if let Ok(Some(creds)) = state.app.mcp_creds().find_by_token_hash(&token_hash).await {
            if !creds.is_revoked() {
                match &creds.owner {
                    drua_core::primitives::McpCredsOwner::User { user_id } => {
                        return AuthSubject::ExportedAgent(
                            *user_id,
                            creds.id,
                            creds.scopes.clone(),
                        );
                    }
                    drua_core::primitives::McpCredsOwner::Agent { agent_id } => {
                        let synthetic_user_id =
                            drua_core::primitives::UserId::from(uuid::Uuid::from(*agent_id));
                        return AuthSubject::ExportedAgent(
                            synthetic_user_id,
                            creds.id,
                            creds.scopes.clone(),
                        );
                    }
                }
            }
        }

        // SA tokens from sandbox pods (projected ServiceAccount tokens).
        if sa_token::looks_like_jwt(&raw_token) {
            if let Some(ref validator) = state.sa_token_validator {
                if let Ok(id_str) = validator.validate(&raw_token).await {
                    let agent_id = domain::primitives::AgentId::from(
                        id_str.parse::<uuid::Uuid>().expect("validated as UUID"),
                    );
                    // System-level User subject bypasses authz; runs during token
                    // resolution, before the per-request AuthSubject is known.
                    let system_sub =
                        AuthSubject::User(domain::primitives::UserId::from(uuid::Uuid::nil()));
                    if let Ok(agent) = state.app.agents().find_by_id(&system_sub, agent_id).await {
                        return agent.auth_subject();
                    }
                }
            }
        }
    }

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
                    Some("dev-user".to_string()),
                )
                .await?
        }
    };

    session.insert("user_id", user.id).await?;
    let redirect_to = session
        .remove::<String>("cli_return_to")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "/".to_string());
    Ok(axum::response::Redirect::to(&redirect_to))
}

#[instrument(name = "web.auth.logout", skip_all)]
async fn logout(session: Session) -> axum::response::Redirect {
    let _ = session.flush().await;
    axum::response::Redirect::to("/")
}

#[instrument(name = "web.auth.cli_login", skip_all)]
async fn cli_login(State(state): State<AppState>, session: Session) -> Result<Response, AuthError> {
    // If logged in, create a long-lived session and return its ID as the CLI token.
    if let Ok(Some(user_id)) = session.get::<UserId>("user_id").await {
        let mut record = Record {
            id: Id::default(),
            data: Default::default(),
            expiry_date: time::OffsetDateTime::now_utc() + time::Duration::days(30),
        };
        record.data.insert(
            "user_id".to_string(),
            serde_json::to_value(user_id).unwrap(),
        );
        state.session_store.create(&mut record).await?;

        let token = record.id.to_string();
        return Ok(CliTokenTemplate { token }.into_response());
    }

    session.insert("cli_return_to", "/auth/cli-login").await?;
    let dev_auth = state.login == crate::auth::config::LoginMethod::Dev;
    Ok(CliLoginTemplate { dev_auth }.into_response())
}
