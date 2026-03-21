pub mod config;
pub mod error;
mod oauth;
pub mod session_store;
pub mod token;

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

impl AuthContext {
    /// Returns the subject user ID for both User and Agent contexts.
    pub fn subject_user_id(&self) -> async_graphql::Result<UserId> {
        match self {
            AuthContext::User(user_id) => Ok(*user_id),
            AuthContext::Agent(_, user_id) => Ok(*user_id),
            AuthContext::Anonymous => Err(async_graphql::Error::new("Authentication required")),
        }
    }
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
/// Requires the router to have [`AppState`] set via `with_state()`.
#[instrument(name = "mcp_gateway.auth.middleware", skip_all)]
pub async fn auth_middleware(
    State(state): State<AppState>,
    session: Session,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_context = resolve_auth_context(&state, &session, &request).await;
    request.extensions_mut().insert(auth_context);
    next.run(request).await
}

/// Tower layer that resolves [`AuthContext`] into request extensions.
///
/// Unlike [`auth_middleware`] (which uses router state), this layer captures
/// the [`AppState`] at construction time, making it composable with any router.
#[derive(Clone)]
pub struct AuthLayer {
    state: AppState,
}

impl AuthLayer {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl<S> tower::Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> AuthService<S> {
        AuthService {
            inner,
            state: self.state.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    state: AppState,
}

impl<S> tower::Service<Request> for AuthService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request) -> Self::Future {
        let state = self.state.clone();
        let inner = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, inner);

        // Extract what we need from the request synchronously before
        // entering the async block (Request body is not Sync).
        let session = request
            .extensions()
            .get::<tower_sessions::Session>()
            .cloned();
        let bearer_token_hash = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(hash_token);

        Box::pin(async move {
            let auth_context =
                resolve_from_extracted(&state, session.as_ref(), bearer_token_hash.as_deref())
                    .await;
            request.extensions_mut().insert(auth_context);
            inner.call(request).await
        })
    }
}

/// Resolves auth context from pre-extracted request data (for [`AuthService`]).
async fn resolve_from_extracted(
    state: &AppState,
    session: Option<&Session>,
    bearer_token_hash: Option<&str>,
) -> AuthContext {
    if let Some(token_hash) = bearer_token_hash {
        match state.agents.find_by_token_hash(token_hash).await {
            Ok(Some(agent)) if !agent.is_revoked() => {
                return AuthContext::Agent(agent.id, agent.user_id);
            }
            _ => {}
        }
    }

    if let Some(session) = session {
        if let Some(auth_context) = resolve_session(session).await {
            return auth_context;
        }
    }

    AuthContext::Anonymous
}

async fn resolve_auth_context(
    state: &AppState,
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

async fn resolve_bearer_token(state: &AppState, request: &Request) -> Option<AuthContext> {
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
