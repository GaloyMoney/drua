use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect},
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use oauth2::{AuthorizationCode, CsrfToken, Scope, TokenResponse};
use serde::Deserialize;
use tower_sessions::Session;
use tracing::instrument;

use super::error::AuthError;
use crate::AppState;

const CSRF_COOKIE_NAME: &str = "oauth_csrf_token";

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
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AuthError> {
    let (auth_url, csrf_token) = state
        .oauth_client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("read:user".to_string()))
        .add_scope(Scope::new("user:email".to_string()))
        .url();

    let cookie = Cookie::build((CSRF_COOKIE_NAME, csrf_token.secret().clone()))
        .path("/")
        .http_only(true)
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .build();

    Ok((jar.add(cookie), Redirect::temporary(auth_url.as_str())))
}

#[instrument(name = "web.auth.github_callback", skip_all)]
pub async fn github_callback(
    State(state): State<AppState>,
    jar: CookieJar,
    session: Session,
    Query(params): Query<CallbackParams>,
) -> Result<impl IntoResponse, AuthError> {
    let stored_csrf = jar.get(CSRF_COOKIE_NAME).map(|c| c.value().to_string());

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
    let user = match state.app.users().find_by_github_id(&github_id_str).await? {
        Some(user) => user,
        None => {
            state
                .app
                .users()
                .create_from_github_login(github_id_str, github_user.email, github_user.name)
                .await?
        }
    };

    session.insert("user_id", user.id).await?;

    let remove_cookie = Cookie::build((CSRF_COOKIE_NAME, ""))
        .path("/")
        .removal()
        .build();

    Ok((jar.add(remove_cookie), Redirect::temporary("/")))
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
