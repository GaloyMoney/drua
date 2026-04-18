//! WebSocket endpoint for tunnel connections from deployment connectors.
//!
//! A connector in a target cluster dials out to `/tunnel/ws`, authenticates
//! via Bearer token (resolved by the existing auth middleware), sends a
//! registration message listing the tools its local MCP servers expose,
//! and then enters a relay loop: the server pushes `CallTool` requests
//! down the WebSocket and the connector returns results.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Response},
    Extension,
};
use tracing::instrument;

use drua_core as domain;

use domain::auth::AuthSubject;
use domain::toolset::SearchableToolSet;
use domain::tunnel::{TunnelHandle, TunnelMessage, TunnelToolSet};

use crate::AppState;

/// HTTP handler — upgrades to WebSocket if the caller is authenticated.
#[instrument(name = "web.tunnel.ws", skip_all)]
pub async fn tunnel_ws_handler(
    ws: WebSocketUpgrade,
    Extension(auth): Extension<AuthSubject>,
    State(state): State<AppState>,
) -> Response {
    match &auth {
        AuthSubject::ExportedAgent(_, _, _) | AuthSubject::User(_) => {}
        _ => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "tunnel requires authentication",
            )
                .into_response()
        }
    }

    ws.on_upgrade(move |socket| handle_tunnel(socket, state, auth))
}

/// Main tunnel lifecycle: registration → relay loop → cleanup.
async fn handle_tunnel(mut socket: WebSocket, state: AppState, _auth: AuthSubject) {
    // ── 1. Wait for registration ──────────────────────────────────────────
    let (deployment_id, toolset_registrations) = match read_registration(&mut socket).await {
        Some(r) => r,
        None => return,
    };

    tracing::info!(
        deployment_id = %deployment_id,
        toolsets = toolset_registrations.len(),
        "tunnel connector registered"
    );

    // ── 2. Create channel and handle ──────────────────────────────────────
    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<String>(256);
    let handle = TunnelHandle::new(outbound_tx);

    // ── 3. Register toolsets ──────────────────────────────────────────────
    let mut registered_names = Vec::new();
    for reg in &toolset_registrations {
        match TunnelToolSet::new(&deployment_id, reg, handle.clone()) {
            Ok(ts) => {
                let name = ts.name().to_string();
                state.app.toolsets().register_searchable(ts);
                registered_names.push(name.clone());
                tracing::info!(
                    deployment_id = %deployment_id,
                    toolset = %reg.name,
                    tools = reg.tools.len(),
                    registered_as = %name,
                    "tunnel toolset registered"
                );
            }
            Err(e) => {
                tracing::warn!(
                    deployment_id = %deployment_id,
                    toolset = %reg.name,
                    error = %e,
                    "failed to create tunnel toolset, skipping"
                );
            }
        }
    }

    // ── 4. Relay loop ─────────────────────────────────────────────────────
    loop {
        tokio::select! {
            // Inbound: messages from the connector (tool results)
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_inbound(&handle, &text).await;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!(deployment_id = %deployment_id, "tunnel disconnected");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::error!(deployment_id = %deployment_id, error = %e, "tunnel read error");
                        break;
                    }
                    Some(Ok(Message::Binary(_))) => {}
                }
            }
            // Outbound: tool call requests to send to the connector
            msg = outbound_rx.recv() => {
                match msg {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            tracing::error!(deployment_id = %deployment_id, "tunnel write error");
                            break;
                        }
                    }
                    None => break, // all senders dropped
                }
            }
        }
    }

    // ── 5. Cleanup ────────────────────────────────────────────────────────
    for name in &registered_names {
        state.app.toolsets().unregister_searchable(name);
    }
    tracing::info!(
        deployment_id = %deployment_id,
        toolsets = registered_names.len(),
        "tunnel toolsets unregistered"
    );
}

/// Read and validate the first WebSocket message as a `Register`.
async fn read_registration(
    socket: &mut WebSocket,
) -> Option<(String, Vec<domain::tunnel::RegisteredToolSet>)> {
    let msg = match socket.recv().await {
        Some(Ok(Message::Text(text))) => text,
        _ => {
            tracing::error!("tunnel: expected text registration message");
            return None;
        }
    };

    let parsed: TunnelMessage = match serde_json::from_str(&msg) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "tunnel: invalid registration JSON");
            return None;
        }
    };

    match parsed {
        TunnelMessage::Register {
            deployment_id,
            toolsets,
        } => Some((deployment_id, toolsets)),
        _ => {
            tracing::error!("tunnel: first message must be register");
            None
        }
    }
}

/// Route an inbound message (tool result or error) to the pending request.
async fn handle_inbound(handle: &TunnelHandle, text: &str) {
    let msg: TunnelMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "tunnel: ignoring unparseable inbound message");
            return;
        }
    };

    match msg {
        TunnelMessage::CallToolResult { id, result } => match serde_json::from_value(result) {
            Ok(call_result) => handle.resolve(&id, Ok(call_result)).await,
            Err(e) => {
                handle
                    .resolve(&id, Err(format!("deserialize result: {e}")))
                    .await
            }
        },
        TunnelMessage::CallToolError { id, error } => {
            handle.resolve(&id, Err(error)).await;
        }
        _ => {
            tracing::warn!("tunnel: unexpected inbound message type");
        }
    }
}
