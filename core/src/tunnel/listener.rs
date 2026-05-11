//! `tunnel_registry_changed` PG NOTIFY listener.
//!
//! Mirrors the `tunnel_registrations` table into per-pod
//! `ProxyTunnelToolSet` entries. Skipped rows (where this pod is the
//! owner) are managed by the WS handler directly via
//! `replace_tunnel_toolsets` / `unregister_searchable_by_session`.
//!
//! Same recv loop / reconnect shape as
//! `spawn_context_generation_listener` — keep them parallel for
//! review-time pattern matching.

use std::sync::Arc;

use sqlx::PgPool;

use crate::toolset::ToolSets;

use super::proxy::{InternalAuth, ProxyTunnelToolSet};
use super::registrations::{TunnelRegistrations, TUNNEL_NOTIFY_CHANNEL};
use super::TunnelRegistry;

/// Spawn the listener task. `self_pod_addr` is `None` in single-replica
/// / local-dev mode — every row is treated as owned-elsewhere, which
/// is fine because the WS handler still does its own in-memory install
/// and the listener's `install_proxy_toolsets` no-ops while a Local
/// entry exists.
///
/// `tunnels` is the in-memory `TunnelRegistry` — needed so the listener
/// can evict a stale Local on takeover (DB row's owner moved to a peer)
/// rather than serving tool calls through a displaced WebSocket.
#[allow(clippy::too_many_arguments)]
pub fn spawn_tunnel_registry_listener(
    pool: PgPool,
    self_pod_addr: Option<String>,
    toolsets: Arc<ToolSets>,
    tunnels: Arc<TunnelRegistry>,
    http: Arc<reqwest::Client>,
    auth: Arc<InternalAuth>,
) {
    tokio::spawn(async move {
        let regs = TunnelRegistrations::new(pool.clone());
        loop {
            let mut listener = match sqlx::postgres::PgListener::connect_with(&pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "tunnel listener: connect failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };
            if let Err(e) = listener.listen(TUNNEL_NOTIFY_CHANNEL).await {
                tracing::warn!(error = %e, "tunnel listener: LISTEN failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }

            // Initial sweep on every (re)connect. Also wipes our local
            // proxies for any deployment whose row is gone, AND evicts
            // any displaced Local sessions whose takeover notify fired
            // during the disconnect window — without the eviction the
            // old owner would keep serving stale tool calls until its
            // next heartbeat tick.
            if let Err(e) = reconcile_all(
                &regs,
                self_pod_addr.as_deref(),
                &toolsets,
                &tunnels,
                &http,
                &auth,
            )
            .await
            {
                tracing::warn!(error = %e, "tunnel listener: initial sweep failed");
            }

            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        let deployment_id = notification.payload();
                        if let Err(e) = reconcile_one(
                            &regs,
                            self_pod_addr.as_deref(),
                            &toolsets,
                            &tunnels,
                            &http,
                            &auth,
                            deployment_id,
                        )
                        .await
                        {
                            tracing::warn!(
                                error = %e,
                                deployment_id = %deployment_id,
                                "tunnel listener: reconcile failed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "tunnel listener: recv failed; reconnecting");
                        break;
                    }
                }
            }
        }
    });
}

/// Reconcile one deployment to match the table state. Public so the
/// WS-handler cleanup path can trigger an immediate reconcile after
/// `unregister_searchable_by_session` clears its Local entries (without
/// this, the next install_proxy attempt would have to wait for an
/// unrelated notify).
///
/// `tunnels` lets us evict a Local entry whose session_id no longer
/// matches the row — that's the cross-pod takeover signal. Without
/// the eviction, the displaced owner keeps serving its old toolsets
/// until its own heartbeat tick (up to `heartbeat_secs`).
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_one(
    regs: &TunnelRegistrations,
    self_pod_addr: Option<&str>,
    toolsets: &ToolSets,
    tunnels: &TunnelRegistry,
    http: &Arc<reqwest::Client>,
    auth: &Arc<InternalAuth>,
    deployment_id: &str,
) -> Result<(), sqlx::Error> {
    let row = regs.fetch_active(deployment_id).await?;
    match row {
        None => {
            toolsets.remove_proxy_toolsets(deployment_id);
        }
        Some(row) if Some(row.owner_pod_addr.as_str()) == self_pod_addr => {
            // Owner-self: WS handler authoritative; do nothing.
        }
        Some(row) => {
            // Cross-pod takeover: if we still hold a Local for this
            // deployment whose session_id doesn't match the row,
            // signal it to close. The WS cleanup tail will run a
            // fresh `tunnel_reconcile` after teardown to install
            // the Proxy. `install_proxy_toolsets` here would no-op
            // anyway while Local exists — short-circuit so we don't
            // serialize a Proxy build that will be discarded.
            if tunnels
                .evict_if_session_differs(&row.deployment_id, row.session_id)
                .await
            {
                tracing::info!(
                    deployment_id = %row.deployment_id,
                    new_owner = %row.owner_pod_addr,
                    "evicted local Local on takeover; deferring Proxy install"
                );
                return Ok(());
            }

            let proxies = ProxyTunnelToolSet::build(
                &row.deployment_id,
                row.session_id,
                &row.owner_pod_addr,
                &row.toolsets,
                http.clone(),
                auth.clone(),
            );
            let arcs: Vec<Arc<dyn crate::toolset::SearchableToolSet>> = proxies
                .into_iter()
                .map(|p| Arc::new(p) as Arc<dyn crate::toolset::SearchableToolSet>)
                .collect();
            toolsets.install_proxy_toolsets(&row.deployment_id, arcs);
        }
    }
    Ok(())
}

/// Sweep every active row and reconcile this pod's view to match.
/// Called once on each (re)connect of the listener — a notify lost
/// during the disconnect window is recovered here. Public so tests
/// can pin the full-sweep eviction path; production code calls it
/// only via the listener task.
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_all(
    regs: &TunnelRegistrations,
    self_pod_addr: Option<&str>,
    toolsets: &ToolSets,
    tunnels: &TunnelRegistry,
    http: &Arc<reqwest::Client>,
    auth: &Arc<InternalAuth>,
) -> Result<(), sqlx::Error> {
    let rows = regs.fetch_all_active().await?;
    let active_ids: std::collections::HashSet<String> =
        rows.iter().map(|r| r.deployment_id.clone()).collect();

    // Drop proxies whose row vanished while this listener was offline
    // (e.g. reaper deletion + pg_notify lost across the disconnect).
    // Without this, peer pods can keep advertising dead tunnels after
    // a flapping DB connection.
    for stale in toolsets.proxy_deployment_ids().difference(&active_ids) {
        toolsets.remove_proxy_toolsets(stale);
    }

    for row in rows {
        if Some(row.owner_pod_addr.as_str()) == self_pod_addr {
            continue;
        }
        // Same takeover-eviction as `reconcile_one` — covers the case
        // where the takeover's pg_notify fired during this listener's
        // disconnect window, so the displaced Local would otherwise
        // keep serving until its heartbeat tick.
        if tunnels
            .evict_if_session_differs(&row.deployment_id, row.session_id)
            .await
        {
            tracing::info!(
                deployment_id = %row.deployment_id,
                new_owner = %row.owner_pod_addr,
                "evicted local Local during full sweep; deferring Proxy install"
            );
            continue;
        }
        let proxies = ProxyTunnelToolSet::build(
            &row.deployment_id,
            row.session_id,
            &row.owner_pod_addr,
            &row.toolsets,
            http.clone(),
            auth.clone(),
        );
        let arcs: Vec<Arc<dyn crate::toolset::SearchableToolSet>> = proxies
            .into_iter()
            .map(|p| Arc::new(p) as Arc<dyn crate::toolset::SearchableToolSet>)
            .collect();
        toolsets.install_proxy_toolsets(&row.deployment_id, arcs);
    }
    Ok(())
}
