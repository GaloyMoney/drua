//! WebSocket endpoint for tunnel connections from deployment connectors.
//!
//! The connector in a target cluster dials out to `/tunnel/ws` carrying
//! a signed `Authorization: Tunnel …` header (see [`verify_handshake`]).
//! drua looks up the corresponding `TunnelDeployment` public key in
//! config-driven state, verifies the Ed25519 signature over a fresh
//! timestamp, and only then upgrades the socket and enters the relay
//! loop.
//!
//! Intentionally decoupled from [`crate::auth`]: this endpoint is **not**
//! routed through `auth_middleware`, and a connector's identity is
//! **not** carried by any MCP credential. A deployment is its own
//! first-class principal — see `config.server.tunnel.deployments`.

use std::collections::HashMap;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use tracing::instrument;

use drua_core as domain;

use domain::toolset::SearchableToolSet;
use domain::tunnel::{TunnelHandle, TunnelMessage, TunnelToolSet};

use crate::config::TunnelConfig;
use crate::AppState;

/// Max clock skew between connector and drua on the handshake timestamp.
/// Anything outside this window is rejected as a replay / stale signature.
const HANDSHAKE_MAX_SKEW_MS: i64 = 60_000;

/// Parse the `TunnelConfig.deployments` map from config (base64 strings)
/// into pre-verified [`VerifyingKey`]s, so the handshake path does zero
/// base64 decoding per request. Fails loudly at boot on any malformed
/// entry — better than silently rejecting that deployment's handshakes
/// forever.
pub fn parse_configured_keys(cfg: &TunnelConfig) -> anyhow::Result<HashMap<String, VerifyingKey>> {
    let mut out = HashMap::with_capacity(cfg.deployments.len());
    for (deployment_id, b64) in &cfg.deployments {
        let raw = URL_SAFE_NO_PAD.decode(b64).map_err(|e| {
            anyhow::anyhow!("tunnel deployment '{deployment_id}': base64 decode: {e}")
        })?;
        let bytes: [u8; 32] = raw.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!(
                "tunnel deployment '{deployment_id}': expected 32-byte ed25519 key, got {}",
                v.len()
            )
        })?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|e| {
            anyhow::anyhow!("tunnel deployment '{deployment_id}': invalid ed25519 key: {e}")
        })?;
        out.insert(deployment_id.clone(), key);
    }
    Ok(out)
}

/// Outcome of validating the tunnel handshake header.
///
/// On success carries only the verified `deployment_id` — that's the
/// entire identity a connector has on drua's side. There is no scope
/// list, no user, no agent; the downstream relay code takes
/// `deployment_id` as the sole authorization input.
#[derive(Debug)]
pub enum HandshakeOutcome {
    Verified { deployment_id: String },
    Rejected { reason: &'static str },
}

/// Verify a tunnel handshake header against the configured public keys.
///
/// Header grammar:
///
/// ```text
/// Authorization: Tunnel <deployment_id>:<timestamp_ms>:<base64_signature>
/// ```
///
/// where `signature = Ed25519.sign(private_key, deployment_id || "|" || timestamp_ms)`.
///
/// Rejects if the header is malformed, the `deployment_id` is unknown,
/// the timestamp is outside [`HANDSHAKE_MAX_SKEW_MS`], or the signature
/// doesn't verify. The rejection reason is intentionally coarse ("bad
/// signature", "unknown deployment", …) — a detailed error would leak
/// information useful for probing.
pub fn verify_handshake(
    headers: &HeaderMap,
    public_keys: &HashMap<String, VerifyingKey>,
) -> HandshakeOutcome {
    let header = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) => v,
        None => {
            return HandshakeOutcome::Rejected {
                reason: "missing authorization header",
            }
        }
    };

    let rest = match header.strip_prefix("Tunnel ") {
        Some(r) => r,
        None => {
            return HandshakeOutcome::Rejected {
                reason: "expected 'Tunnel' auth scheme",
            }
        }
    };

    // `deployment_id:timestamp_ms:signature`. Split from the right so
    // a (future) deployment_id that accidentally contains a colon
    // still parses — the signature is base64 (no colons) and the
    // timestamp is ascii digits (no colons), so the two rightmost
    // colons are unambiguous.
    let (ts_and_id, signature_b64) = match rest.rsplit_once(':') {
        Some(pair) => pair,
        None => {
            return HandshakeOutcome::Rejected {
                reason: "malformed header",
            }
        }
    };
    let (deployment_id, timestamp_ms_str) = match ts_and_id.rsplit_once(':') {
        Some(pair) => pair,
        None => {
            return HandshakeOutcome::Rejected {
                reason: "malformed header",
            }
        }
    };

    let timestamp_ms: i64 = match timestamp_ms_str.parse() {
        Ok(t) => t,
        Err(_) => {
            return HandshakeOutcome::Rejected {
                reason: "malformed timestamp",
            }
        }
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    if (now_ms - timestamp_ms).abs() > HANDSHAKE_MAX_SKEW_MS {
        return HandshakeOutcome::Rejected {
            reason: "timestamp outside replay window",
        };
    }

    let public_key = match public_keys.get(deployment_id) {
        Some(k) => k,
        None => {
            return HandshakeOutcome::Rejected {
                reason: "unknown deployment",
            }
        }
    };

    let signature_bytes = match URL_SAFE_NO_PAD.decode(signature_b64) {
        Ok(b) => b,
        Err(_) => {
            return HandshakeOutcome::Rejected {
                reason: "signature not base64",
            }
        }
    };
    let signature_arr: [u8; 64] = match signature_bytes.try_into() {
        Ok(a) => a,
        Err(_) => {
            return HandshakeOutcome::Rejected {
                reason: "signature wrong length",
            }
        }
    };
    let signature = Signature::from_bytes(&signature_arr);

    let signed_payload = format!("{deployment_id}|{timestamp_ms}");
    match public_key.verify(signed_payload.as_bytes(), &signature) {
        Ok(()) => HandshakeOutcome::Verified {
            deployment_id: deployment_id.to_string(),
        },
        Err(_) => HandshakeOutcome::Rejected {
            reason: "signature verification failed",
        },
    }
}

/// HTTP handler — verifies handshake, then upgrades to WebSocket.
///
/// This route does **not** flow through `auth_middleware` (see
/// [`crate::routes::api_router`]); the handshake verifier is the only
/// auth layer for this endpoint.
#[instrument(name = "web.tunnel.ws", skip_all)]
pub async fn tunnel_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let deployment_id = match verify_handshake(&headers, &state.tunnel_public_keys) {
        HandshakeOutcome::Verified { deployment_id } => deployment_id,
        HandshakeOutcome::Rejected { reason } => {
            tracing::warn!(reason = %reason, "tunnel handshake rejected");
            return (StatusCode::UNAUTHORIZED, reason).into_response();
        }
    };

    ws.on_upgrade(move |socket| handle_tunnel(socket, state, deployment_id))
}

/// Main tunnel lifecycle: read register frame → relay loop → cleanup.
/// `deployment_id` here is the one the handshake verifier returned —
/// the connector cannot spoof a different one via the register frame
/// because we ignore `deployment_id` from that frame entirely.
async fn handle_tunnel(mut socket: WebSocket, state: AppState, deployment_id: String) {
    // ── 1. Read register frame ────────────────────────────────────────────
    // The handshake already verified *who* the connector is. The register
    // frame just advertises the catalog; any `deployment_id` the frame
    // carries is informational only.
    let toolset_registrations = match read_registration(&mut socket).await {
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

    // ── 2b. Claim deployment_id, evict previous tunnel if any ─────────────
    // Capacity-1 channel: a single eviction signal is all we ever send.
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<()>(1);
    let session_id = uuid::Uuid::new_v4();
    let evicted = state
        .app
        .tunnels()
        .claim(&deployment_id, session_id, close_tx)
        .await;
    if evicted {
        tracing::warn!(
            deployment_id = %deployment_id,
            "evicted previous tunnel with same deployment_id; new connector takes over"
        );
    }

    // ── 3. Build + atomically swap toolsets ───────────────────────────────
    // `replace_tunnel_toolsets` retains any evicted session's entries out
    // of the catalog and appends the new ones under a single write lock,
    // so (a) first-match routing never sees stale entries for this
    // deployment, and (b) the evicted loop's later session-scoped
    // cleanup is a no-op on the new entries.
    let mut new_sets: Vec<std::sync::Arc<dyn SearchableToolSet>> =
        Vec::with_capacity(toolset_registrations.len());
    for reg in &toolset_registrations {
        match TunnelToolSet::new(&deployment_id, session_id, reg, handle.clone()) {
            Ok(ts) => {
                tracing::info!(
                    deployment_id = %deployment_id,
                    toolset = %reg.name,
                    tools = reg.tools.len(),
                    registered_as = %ts.name(),
                    "tunnel toolset prepared"
                );
                new_sets.push(std::sync::Arc::new(ts));
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
    let registered_count = new_sets.len();
    state
        .app
        .toolsets()
        .replace_tunnel_toolsets(&deployment_id, new_sets);

    // ── 4. Relay loop ─────────────────────────────────────────────────────
    loop {
        tokio::select! {
            // `biased` so an eviction signal always beats in-flight traffic.
            biased;
            // Eviction: another connector claimed the same deployment_id.
            _ = close_rx.recv() => {
                tracing::info!(
                    deployment_id = %deployment_id,
                    "tunnel evicted by new registration for the same deployment_id; closing"
                );
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::POLICY,
                        reason: "evicted by a new tunnel registration for the same deployment_id".into(),
                    })))
                    .await;
                break;
            }
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
    // Ordering matters here:
    //
    //   1. `unregister_searchable_by_session` — removes our entries from the
    //      catalog so no *new* tool calls can reach our handle. Session-scoped
    //      so an already-evicted loop (whose entries were replaced by a newer
    //      connector via `replace_tunnel_toolsets`) is a safe no-op.
    //
    //   2. `fail_all_pending` — drains any call that slipped in between the
    //      tunnel going down and the unregister completing. Without this,
    //      those callers wait the full 120s timeout for a response that
    //      will never arrive (the `TunnelHandle` clones inside the now-
    //      unregistered `TunnelToolSet`s kept the pending map alive).
    //
    //   3. `TunnelRegistry::release` — same session_id invariant as above.
    state
        .app
        .toolsets()
        .unregister_searchable_by_session(session_id);
    handle.fail_all_pending("tunnel disconnected").await;
    state.app.tunnels().release(&deployment_id, session_id);
    tracing::info!(
        deployment_id = %deployment_id,
        toolsets = registered_count,
        "tunnel toolsets unregistered"
    );
}

/// Read the register frame. The `deployment_id` in the payload is
/// ignored; the authoritative identity comes from the handshake.
async fn read_registration(
    socket: &mut WebSocket,
) -> Option<Vec<domain::tunnel::RegisteredToolSet>> {
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
        TunnelMessage::Register { toolsets, .. } => Some(toolsets),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_key() -> (SigningKey, VerifyingKey, String) {
        let signing = SigningKey::generate(&mut rand::thread_rng());
        let verifying = signing.verifying_key();
        let b64 = URL_SAFE_NO_PAD.encode(verifying.to_bytes());
        (signing, verifying, b64)
    }

    fn sign_header(deployment_id: &str, signing: &SigningKey, ts_ms: i64) -> String {
        let payload = format!("{deployment_id}|{ts_ms}");
        let sig = signing.sign(payload.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("Tunnel {deployment_id}:{ts_ms}:{sig_b64}")
    }

    fn hdrs(auth: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(auth).unwrap(),
        );
        h
    }

    #[test]
    fn verified_on_fresh_signature() {
        let (signing, verifying, _) = make_key();
        let mut keys = HashMap::new();
        keys.insert("galoy-staging".to_string(), verifying);
        let now = chrono::Utc::now().timestamp_millis();
        let header = sign_header("galoy-staging", &signing, now);

        match verify_handshake(&hdrs(&header), &keys) {
            HandshakeOutcome::Verified { deployment_id } => {
                assert_eq!(deployment_id, "galoy-staging");
            }
            HandshakeOutcome::Rejected { reason } => {
                panic!("expected verify, got rejection: {reason}")
            }
        }
    }

    #[test]
    fn rejects_stale_timestamp() {
        let (signing, verifying, _) = make_key();
        let mut keys = HashMap::new();
        keys.insert("galoy-staging".to_string(), verifying);
        // Far outside the replay window.
        let stale = chrono::Utc::now().timestamp_millis() - HANDSHAKE_MAX_SKEW_MS - 1_000;
        let header = sign_header("galoy-staging", &signing, stale);

        assert!(matches!(
            verify_handshake(&hdrs(&header), &keys),
            HandshakeOutcome::Rejected {
                reason: "timestamp outside replay window"
            }
        ));
    }

    #[test]
    fn rejects_unknown_deployment() {
        let (signing, _, _) = make_key();
        let keys = HashMap::new();
        let now = chrono::Utc::now().timestamp_millis();
        let header = sign_header("galoy-staging", &signing, now);

        assert!(matches!(
            verify_handshake(&hdrs(&header), &keys),
            HandshakeOutcome::Rejected {
                reason: "unknown deployment"
            }
        ));
    }

    #[test]
    fn rejects_wrong_key_signature() {
        let (_signing_a, verifying_a, _) = make_key();
        let (signing_b, _verifying_b, _) = make_key();
        // drua thinks the deployment's key is A, but the client signs with B.
        let mut keys = HashMap::new();
        keys.insert("galoy-staging".to_string(), verifying_a);
        let now = chrono::Utc::now().timestamp_millis();
        let header = sign_header("galoy-staging", &signing_b, now);

        assert!(matches!(
            verify_handshake(&hdrs(&header), &keys),
            HandshakeOutcome::Rejected {
                reason: "signature verification failed"
            }
        ));
    }

    #[test]
    fn rejects_malformed_header() {
        let keys = HashMap::new();
        for raw in [
            "",
            "Bearer drua_xxx",
            "Tunnel ",
            "Tunnel galoy-staging:notatimestamp:sig",
            "Tunnel noseparators",
            "Tunnel a:b",
        ] {
            let rejected = matches!(
                verify_handshake(&hdrs(raw), &keys),
                HandshakeOutcome::Rejected { .. }
            );
            assert!(rejected, "expected reject for: {raw:?}");
        }
    }
}
