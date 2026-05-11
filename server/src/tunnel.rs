//! `/tunnel/ws` — signed-handshake auth, then relay. Does not flow
//! through `auth_middleware`; identity is a `deployment_id` verified
//! via [`verify_handshake`] against `config.server.tunnel.deployments`.

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
use ed25519_dalek::{pkcs8::DecodePublicKey, Signature, Verifier, VerifyingKey};
use tracing::instrument;

use drua_core as domain;

use domain::toolset::SearchableToolSet;
use domain::tunnel::{LocalTunnelToolSet, TunnelHandle, TunnelMessage};

use crate::config::TunnelConfig;
use crate::AppState;

/// Replay-window for the handshake timestamp, in either direction.
const HANDSHAKE_MAX_SKEW_MS: i64 = 60_000;

/// Decode every config entry to a [`VerifyingKey`] at boot; fail loudly
/// on any bad entry rather than silently rejecting its handshakes.
pub fn parse_configured_keys(cfg: &TunnelConfig) -> anyhow::Result<HashMap<String, VerifyingKey>> {
    let mut out = HashMap::with_capacity(cfg.deployments.len());
    for (deployment_id, pem) in &cfg.deployments {
        let key = VerifyingKey::from_public_key_pem(pem).map_err(|e| {
            anyhow::anyhow!("tunnel deployment '{deployment_id}': not a valid Ed25519 SubjectPublicKeyInfo PEM: {e}")
        })?;
        out.insert(deployment_id.clone(), key);
    }
    Ok(out)
}

#[derive(Debug)]
pub enum HandshakeOutcome {
    Verified { deployment_id: String },
    Rejected { reason: &'static str },
}

/// Parse `Authorization: Tunnel <deployment_id>:<ts_ms>:<sig_b64>` and
/// verify the Ed25519 signature (over `deployment_id || "|" || ts_ms`)
/// against the configured key for `deployment_id`. Rejection reasons
/// are deliberately coarse to avoid leaking probe-useful info.
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

    // Split from the right: signature and ts contain no colons, so
    // the two rightmost colons are unambiguous even if a deployment_id
    // ever contains one.
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

#[instrument(name = "web.tunnel.project", skip_all)]
pub async fn tunnel_ws_handler(
    project: WebSocketUpgrade,
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

    project.on_upgrade(move |socket| handle_tunnel(socket, state, deployment_id))
}

/// `deployment_id` comes from the already-verified handshake; any
/// `deployment_id` field in the register frame is ignored.
async fn handle_tunnel(mut socket: WebSocket, state: AppState, deployment_id: String) {
    let toolset_registrations = match read_registration(&mut socket).await {
        Some(r) => r,
        None => return,
    };

    tracing::info!(
        deployment_id = %deployment_id,
        toolsets = toolset_registrations.len(),
        "tunnel connector registered"
    );

    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::channel::<String>(256);
    let handle = TunnelHandle::new(outbound_tx);

    // Capacity-1 channel: a single eviction signal is all we ever send.
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<()>(1);
    let session_id = uuid::Uuid::new_v4();
    let evicted = state
        .app
        .tunnels()
        .claim(&deployment_id, session_id, close_tx, handle.clone())
        .await;
    if evicted {
        tracing::warn!(
            deployment_id = %deployment_id,
            "evicted previous tunnel with same deployment_id; new connector takes over"
        );
    }

    // Cross-pod: stake ownership in `tunnel_registrations` before
    // installing toolsets so a peer pod's reconcile can never observe
    // an empty row for a deployment we're about to serve. If the
    // upsert fails we bail rather than serve a deployment that peer
    // pods can't see — the connector's exponential backoff retries.
    let runtime = state.app.tunnel_runtime().clone();
    let upsert_target = runtime.self_pod_addr.clone();
    if let Some(self_addr) = upsert_target.as_deref() {
        let ttl = std::time::Duration::from_secs(runtime.expires_after_secs);
        if let Err(e) = state
            .app
            .tunnel_registrations()
            .upsert(
                &deployment_id,
                session_id,
                self_addr,
                &toolset_registrations,
                ttl,
            )
            .await
        {
            tracing::error!(
                deployment_id = %deployment_id,
                error = %e,
                "tunnel registrations upsert failed; refusing connector"
            );
            state.app.tunnels().release(&deployment_id, session_id);
            return;
        }
    }

    // `replace_tunnel_toolsets` retains any evicted session's entries out
    // of the catalog and appends the new ones under a single write lock,
    // so (a) first-match routing never sees stale entries for this
    // deployment, and (b) the evicted loop's later session-scoped
    // cleanup is a no-op on the new entries.
    let mut new_sets: Vec<std::sync::Arc<dyn SearchableToolSet>> =
        Vec::with_capacity(toolset_registrations.len());
    for reg in &toolset_registrations {
        match LocalTunnelToolSet::new(&deployment_id, session_id, reg, handle.clone()) {
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

    // Heartbeat extends `expires_at` while we hold the WS. Returns
    // `Ok(false)` when a peer pod's UPSERT changed `session_id` —
    // that's the cross-pod displacement signal; we close the WS.
    let heartbeat_state = upsert_target.as_deref().map(|_| {
        spawn_heartbeat(
            state.app.tunnel_registrations().clone(),
            deployment_id.clone(),
            session_id,
            std::time::Duration::from_secs(runtime.heartbeat_secs),
            std::time::Duration::from_secs(runtime.expires_after_secs),
        )
    });
    let (heartbeat_handle, mut displaced_rx) = match heartbeat_state {
        Some((h, rx)) => (Some(h), Some(rx)),
        None => (None, None),
    };

    loop {
        let displaced_fut = async {
            match displaced_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending::<Option<()>>().await,
            }
        };
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
            // Cross-pod displacement: heartbeat saw rows-affected=0,
            // meaning a peer pod's UPSERT replaced our session_id.
            _ = displaced_fut => {
                tracing::info!(
                    deployment_id = %deployment_id,
                    "tunnel displaced by peer pod's registration; closing"
                );
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::POLICY,
                        reason: "displaced by a peer pod's registration".into(),
                    })))
                    .await;
                break;
            }
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

    // Stop the heartbeat task so it doesn't fire one last UPDATE
    // after we've released ownership.
    if let Some(h) = heartbeat_handle {
        h.abort();
    }

    // Cleanup ordering matters:
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
    //      unregistered `LocalTunnelToolSet`s kept the pending map alive).
    //
    //   3. `TunnelRegistry::release` — same session_id invariant as above.
    state
        .app
        .toolsets()
        .unregister_searchable_by_session(session_id);
    handle.fail_all_pending("tunnel disconnected").await;
    state.app.tunnels().release(&deployment_id, session_id);

    // Drop our DB row so peer pods stop routing here. delete_if_owner
    // is a no-op if a takeover already replaced our session_id; in
    // that case the new owner already owns the row.
    if upsert_target.is_some() {
        match state
            .app
            .tunnel_registrations()
            .delete_if_owner(&deployment_id, session_id)
            .await
        {
            Ok(0) => tracing::debug!(
                deployment_id = %deployment_id,
                "DB row was already taken over before disconnect"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                deployment_id = %deployment_id,
                error = %e,
                "tunnel registration delete failed"
            ),
        }
    }

    // Tail-side reconcile: after our Local entries are gone, install
    // a Proxy if the row has moved to a peer pod (the takeover case).
    // No-op if the row is genuinely gone (graceful disconnect).
    if upsert_target.is_some() {
        if let Err(e) = state.app.tunnel_reconcile(&deployment_id).await {
            tracing::warn!(
                deployment_id = %deployment_id,
                error = %e,
                "post-cleanup tunnel reconcile failed"
            );
        }
    }

    tracing::info!(
        deployment_id = %deployment_id,
        toolsets = registered_count,
        "tunnel toolsets unregistered"
    );
}

/// Background task that bumps `expires_at` every `heartbeat_every`.
/// Returns `(abort_handle, displaced_rx)`: the abort handle so the WS
/// loop can stop the task on cleanup; the receiver yields once if the
/// row was taken over by a peer pod (rows-affected = 0). Transient DB
/// errors are logged and retried; only an explicit rows-affected=0
/// means "displaced".
fn spawn_heartbeat(
    regs: drua_core::tunnel::TunnelRegistrations,
    deployment_id: String,
    session_id: uuid::Uuid,
    heartbeat_every: std::time::Duration,
    ttl: std::time::Duration,
) -> (tokio::task::AbortHandle, tokio::sync::mpsc::Receiver<()>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(heartbeat_every);
        // First tick fires immediately — skip it; we just upserted.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match regs.heartbeat(&deployment_id, session_id, ttl).await {
                Ok(true) => continue,
                Ok(false) => {
                    let _ = tx.try_send(());
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        deployment_id = %deployment_id,
                        error = %e,
                        "tunnel heartbeat error; will retry"
                    );
                }
            }
        }
    });
    (handle.abort_handle(), rx)
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
    use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, EncodePublicKey};
    use ed25519_dalek::{Signer, SigningKey};

    /// Returns the keypair along with a PEM encoding of the public half,
    /// matching the shape `parse_configured_keys` will see in real
    /// config (emitted by Terraform `tls_private_key`).
    fn make_key() -> (SigningKey, VerifyingKey, String) {
        let signing = SigningKey::generate(&mut rand::thread_rng());
        let verifying = signing.verifying_key();
        let pem = verifying
            .to_public_key_pem(LineEnding::LF)
            .expect("encode public key pem");
        (signing, verifying, pem)
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

    /// End-to-end for the config path: parse the same PEM the test
    /// helper emits, and use the resulting `VerifyingKey` to check a
    /// fresh signature. Proves the PEM → VerifyingKey hop.
    #[test]
    fn parse_configured_keys_round_trips_pem() {
        let (signing, _, pem) = make_key();
        let mut cfg = TunnelConfig::default();
        cfg.deployments.insert("galoy-staging".to_string(), pem);

        let keys = parse_configured_keys(&cfg).expect("parse");
        assert_eq!(keys.len(), 1);

        let now = chrono::Utc::now().timestamp_millis();
        let header = sign_header("galoy-staging", &signing, now);
        assert!(matches!(
            verify_handshake(&hdrs(&header), &keys),
            HandshakeOutcome::Verified { .. }
        ));
    }

    #[test]
    fn parse_configured_keys_rejects_bad_pem() {
        let mut cfg = TunnelConfig::default();
        cfg.deployments
            .insert("galoy-staging".to_string(), "not a pem".to_string());
        assert!(parse_configured_keys(&cfg).is_err());
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
