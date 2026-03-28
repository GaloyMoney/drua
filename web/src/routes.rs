use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post},
    Extension, Form, Json, Router,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower_sessions::Session;
use tracing::instrument;

use galoy_agents_core as domain;

use domain::agent::token::generate_token;
use domain::agent::Agent;
use domain::auth::AuthContext;
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
        .route("/style-agent/search", get(style_agent_search))
        .route("/sandboxes", get(sandboxes_page))
        .route("/sandboxes/list", get(sandbox_list))
        .route("/sandboxes/create", post(sandbox_create))
        .route("/sandboxes/{name}/delete", post(sandbox_delete))
        .route("/sandboxes/{name}/status", get(sandbox_status))
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

fn build_mcp_config(mcp_endpoint: &str, token: &str) -> (String, String) {
    let server_config = serde_json::json!({
        "type": "http",
        "url": mcp_endpoint,
        "headers": {
            "Authorization": format!("Bearer {token}")
        }
    });
    let mcp_json = serde_json::json!({
        "galoy-agents": &server_config
    })
    .to_string();
    let cli_command = format!(
        "claude mcp add-json --scope user galoy-agents '{}'",
        server_config
    );
    (mcp_json, cli_command)
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
            let (mcp_json, cli_command) = build_mcp_config(&state.mcp_endpoint, &raw_token);
            AgentCreatedTemplate {
                agent_name: form.name,
                mcp_json,
                cli_command,
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

    let rows = match state.app.style_agent_logs().recent_requests(10).await {
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

    let rows = match state.app.style_agent_logs().least_useful(10).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load least useful requests");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    StyleAgentRecentTemplate { rows }.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    label: Option<String>,
}

#[instrument(name = "web.style_agent_search", skip_all)]
async fn style_agent_search(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<SearchParams>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let query = params.q.unwrap_or_default();
    if query.is_empty() {
        return StyleAgentSearchResultsTemplate {
            query,
            results: vec![],
            error: None,
        }
        .into_response();
    }

    let endpoints = match state.style_agent.as_ref() {
        Some(ep) => ep,
        None => {
            return StyleAgentSearchResultsTemplate {
                query,
                results: vec![],
                error: Some("Style-agent is not configured".to_string()),
            }
            .into_response();
        }
    };

    let label = params.label.as_deref().filter(|s| !s.is_empty());

    match endpoints.search_raw(&query, 10, label).await {
        Ok(raw_results) => {
            let results = raw_results
                .iter()
                .map(|r| SearchResultView {
                    file_path: r.file_path.clone(),
                    repo: r.repo.clone(),
                    score: format!("{:.3}", r.score),
                    labels: r.labels.join(", "),
                    lines: format!("{}-{}", r.line_start, r.line_end),
                    content: r.content.clone(),
                    language: if r.language.is_empty() {
                        "rust".to_string()
                    } else {
                        r.language.clone()
                    },
                })
                .collect();
            StyleAgentSearchResultsTemplate {
                query,
                results,
                error: None,
            }
            .into_response()
        }
        Err(e) => StyleAgentSearchResultsTemplate {
            query,
            results: vec![],
            error: Some(e.to_string()),
        }
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Sandboxes
// ---------------------------------------------------------------------------

fn summary_to_view(s: &sandbox_client::SandboxSummary) -> SandboxView {
    SandboxView {
        name: s.name.clone(),
        sandbox_name: s.sandbox_name.clone().unwrap_or_default(),
        phase: s.phase.clone(),
    }
}

#[instrument(name = "web.sandboxes_page", skip_all)]
async fn sandboxes_page(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    SandboxesTemplate {
        enabled: state.sandbox.is_some(),
    }
    .into_response()
}

#[instrument(name = "web.sandbox_list", skip_all)]
async fn sandbox_list(State(state): State<AppState>, session: Session) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    let client = match state.sandbox.as_ref() {
        Some(c) => c,
        None => return SandboxListTemplate { sandboxes: vec![] }.into_response(),
    };
    match client.list_claims().await {
        Ok(summaries) => SandboxListTemplate {
            sandboxes: summaries.iter().map(summary_to_view).collect(),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to list sandboxes");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct CreateSandboxForm {
    name: String,
}

#[instrument(name = "web.sandbox_create", skip_all)]
async fn sandbox_create(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<CreateSandboxForm>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    let client = match state.sandbox.as_ref() {
        Some(c) => c,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    match client.create_claim(&form.name).await {
        Ok(_) => SandboxCreatedTemplate { name: form.name }.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to create sandbox");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "web.sandbox_delete", skip_all)]
async fn sandbox_delete(
    State(state): State<AppState>,
    session: Session,
    Path(name): Path<String>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return Redirect::to("/").into_response();
    }
    let client = match state.sandbox.as_ref() {
        Some(c) => c,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    match client.delete_claim(&name).await {
        Ok(_) => "".into_response(), // Row disappears via hx-swap="outerHTML"
        Err(e) => {
            tracing::error!(error = %e, name = %name, "Failed to delete sandbox");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "web.sandbox_status", skip_all)]
async fn sandbox_status(
    State(state): State<AppState>,
    session: Session,
    Path(name): Path<String>,
) -> Response {
    if extract_user_id(&session).await.is_none() {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    let client = match state.sandbox.as_ref() {
        Some(c) => c,
        None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    match client.get_claim(&name).await {
        Ok(summary) => SandboxRowTemplate {
            sb: summary_to_view(&summary),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, name = %name, "Failed to get sandbox status");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// JSON API  (/api/v1/sandboxes)
// ---------------------------------------------------------------------------

/// Wire protocol frame types for WebSocket exec proxy.
mod ws_proto {
    pub const STDIN: u8 = 0x01;
    pub const STDOUT: u8 = 0x02;
    #[allow(dead_code)]
    pub const STDERR: u8 = 0x03;
    pub const RESIZE: u8 = 0x04;
    pub const EXIT: u8 = 0xFF;
}

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/sandboxes", post(api_create_sandbox))
        .route("/api/v1/sandboxes", get(api_list_sandboxes))
        .route("/api/v1/sandboxes/_debug", get(api_debug_sandboxes))
        .route("/api/v1/sandboxes/{name}", get(api_get_sandbox))
        .route("/api/v1/sandboxes/{name}", delete(api_delete_sandbox))
        .route("/api/v1/sandboxes/{name}/exec", get(api_exec_sandbox))
}

macro_rules! require_auth {
    ($auth:expr) => {
        match $auth {
            AuthContext::Agent(_, _) | AuthContext::User(_) => {}
            AuthContext::Anonymous => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
        }
    };
}

macro_rules! require_sandbox {
    ($state:expr) => {
        match $state.sandbox.as_ref() {
            Some(c) => c,
            None => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    };
}

#[instrument(name = "api.sandbox.debug", skip_all)]
async fn api_debug_sandboxes(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
) -> Response {
    require_auth!(auth);
    let client = require_sandbox!(state);
    match client.debug_status().await {
        Ok(info) => Json(info).into_response(),
        Err(e) => {
            let body = serde_json::json!({ "error": e.to_string() });
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct CreateSandboxRequest {
    name: String,
}

#[instrument(name = "api.sandbox.create", skip_all)]
async fn api_create_sandbox(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreateSandboxRequest>,
) -> Response {
    require_auth!(auth);
    let client = require_sandbox!(state);
    match client.create_claim(&body.name).await {
        Ok(_) => {
            let summary = sandbox_client::SandboxSummary {
                name: body.name,
                sandbox_name: None,
                phase: "Pending".to_string(),
                ready: false,
            };
            (axum::http::StatusCode::CREATED, Json(summary)).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "API: failed to create sandbox");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "api.sandbox.list", skip_all)]
async fn api_list_sandboxes(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
) -> Response {
    require_auth!(auth);
    let client = require_sandbox!(state);
    match client.list_claims().await {
        Ok(summaries) => Json(summaries).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "API: failed to list sandboxes");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "api.sandbox.get", skip_all, fields(%name))]
async fn api_get_sandbox(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    require_auth!(auth);
    let client = require_sandbox!(state);
    match client.get_claim(&name).await {
        Ok(summary) => Json(summary).into_response(),
        Err(sandbox_client::SandboxError::NotFound(_)) => {
            axum::http::StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "API: failed to get sandbox");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[instrument(name = "api.sandbox.delete", skip_all, fields(%name))]
async fn api_delete_sandbox(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    require_auth!(auth);
    let client = require_sandbox!(state);
    match client.delete_claim(&name).await {
        Ok(_) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "API: failed to delete sandbox");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Query parameters for the exec WebSocket endpoint.
#[derive(Deserialize)]
struct ExecParams {
    /// Shell command to execute (defaults to "sh").
    cmd: Option<String>,
}

#[instrument(name = "api.sandbox.exec", skip_all, fields(%name))]
async fn api_exec_sandbox(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<ExecParams>,
    ws: WebSocketUpgrade,
) -> Response {
    require_auth!(auth);
    let client = require_sandbox!(state).clone();

    let cmd = params.cmd.unwrap_or_else(|| "sh".to_string());
    let command: Vec<String> = cmd.split_whitespace().map(String::from).collect();

    ws.on_upgrade(move |socket| exec_proxy(socket, client, name, command))
}

/// Bridge a WebSocket connection to a kube-rs exec session.
async fn exec_proxy(
    socket: WebSocket,
    client: std::sync::Arc<sandbox_client::SandboxClient>,
    claim_name: String,
    command: Vec<String>,
) {
    use futures::stream::StreamExt;

    tracing::info!(claim = %claim_name, "Exec proxy: starting");

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Helper to send an error message back to the client over the WebSocket
    macro_rules! ws_error {
        ($sender:expr, $msg:expr) => {{
            use futures::sink::SinkExt;
            let err_msg = format!("exec proxy error: {}\r\n", $msg);
            let mut frame = Vec::with_capacity(1 + err_msg.len());
            frame.push(ws_proto::STDERR);
            frame.extend_from_slice(err_msg.as_bytes());
            let _ = $sender.send(Message::Binary(frame.into())).await;
            let _ = $sender
                .send(Message::Binary(vec![ws_proto::EXIT, 1].into()))
                .await;
            let _ = $sender.close().await;
        }};
    }

    // Start exec in the sandbox pod
    let mut process = match client.exec_in_sandbox(&claim_name, command).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "Exec proxy: failed to exec into sandbox");
            ws_error!(ws_sender, e);
            return;
        }
    };

    let mut pod_stdin = match process.stdin() {
        Some(s) => s,
        None => {
            tracing::error!("Exec proxy: no stdin stream");
            ws_error!(ws_sender, "no stdin stream from pod");
            return;
        }
    };

    let mut pod_stdout = match process.stdout() {
        Some(s) => s,
        None => {
            tracing::error!("Exec proxy: no stdout stream");
            ws_error!(ws_sender, "no stdout stream from pod");
            return;
        }
    };

    // Pod stdout → WebSocket (0x02 prefix)
    let stdout_task = tokio::spawn(async move {
        use futures::sink::SinkExt;
        let mut buf = [0u8; 4096];
        loop {
            match pod_stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut frame = Vec::with_capacity(1 + n);
                    frame.push(ws_proto::STDOUT);
                    frame.extend_from_slice(&buf[..n]);
                    if ws_sender.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Exec proxy: pod stdout error");
                    // Send exit frame
                    let exit_frame = vec![ws_proto::EXIT, 1];
                    let _ = ws_sender.send(Message::Binary(exit_frame.into())).await;
                    break;
                }
            }
        }
        // Send exit frame on clean close
        let _ = ws_sender
            .send(Message::Binary(vec![ws_proto::EXIT, 0].into()))
            .await;
        let _ = ws_sender.close().await;
    });

    // WebSocket → Pod stdin (strip 0x01 prefix)
    let stdin_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    match data[0] {
                        ws_proto::STDIN => {
                            if pod_stdin.write_all(&data[1..]).await.is_err() {
                                break;
                            }
                        }
                        ws_proto::RESIZE => {
                            // Resize: 0x04 + cols(u16be) + rows(u16be)
                            // kube-rs AttachedProcess doesn't expose resize,
                            // so we log and skip for now.
                            if data.len() >= 5 {
                                let cols = u16::from_be_bytes([data[1], data[2]]);
                                let rows = u16::from_be_bytes([data[3], data[4]]);
                                tracing::debug!(cols, rows, "Exec proxy: resize requested");
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    // Wait for either task to finish, then abort the other
    tokio::select! {
        _ = stdout_task => {}
        _ = stdin_task => {}
    }

    tracing::info!(claim = %claim_name, "Exec proxy: session ended");
}
