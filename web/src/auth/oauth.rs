use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect},
};
use oauth2::{AuthorizationCode, CsrfToken, Scope, TokenResponse};
use serde::Deserialize;
use tower_sessions::Session;
use tracing::instrument;

use super::{error::AuthError, AuthAppState};

const CSRF_SESSION_KEY: &str = "oauth_csrf_token";

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    code: String,
    state: String,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    id: u64,
    email: Option<String>,
    name: Option<String>,
}

#[instrument(name = "web.auth.github_redirect", skip_all)]
pub async fn github_redirect(
    State(state): State<AuthAppState>,
    session: Session,
) -> Result<impl IntoResponse, AuthError> {
    let (auth_url, csrf_token) = state
        .oauth_client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("read:user".to_string()))
        .add_scope(Scope::new("user:email".to_string()))
        .url();

    session
        .insert(CSRF_SESSION_KEY, csrf_token.secret().clone())
        .await?;

    Ok(Redirect::temporary(auth_url.as_str()))
}

#[instrument(name = "web.auth.github_callback", skip_all)]
pub async fn github_callback(
    State(state): State<AuthAppState>,
    session: Session,
    Query(params): Query<CallbackParams>,
) -> Result<impl IntoResponse, AuthError> {
    let stored_csrf: Option<String> = session.get(CSRF_SESSION_KEY).await?;
    session.remove::<String>(CSRF_SESSION_KEY).await?;

    match stored_csrf {
        Some(ref csrf) if csrf == &params.state => {}
        _ => return Err(AuthError::CsrfMismatch),
    }

    let http_client = reqwest::Client::new();
    let token_result = state
        .oauth_client
        .exchange_code(AuthorizationCode::new(params.code))
        .request_async(&http_client)
        .await
        .map_err(|e| AuthError::OAuth(e.to_string()))?;

    let github_user = fetch_github_user(token_result.access_token().secret()).await?;

    let github_id_str = github_user.id.to_string();
    let user = match state.users.find_by_github_id(&github_id_str).await? {
        Some(user) => user,
        None => {
            state
                .users
                .create_from_github_login(github_id_str, github_user.email, github_user.name)
                .await?
        }
    };

    session.insert("user_id", user.id).await?;

    Ok(Redirect::temporary("/"))
}

async fn fetch_github_user(access_token: &str) -> Result<GitHubUser, reqwest::Error> {
    reqwest::Client::new()
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "galoy-agents")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GitHubUser>()
        .await
}
