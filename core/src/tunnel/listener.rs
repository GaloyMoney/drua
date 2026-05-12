use std::sync::Arc;

use sqlx::PgPool;

use crate::toolset::{SearchableToolSet, ToolSets};

use super::proxy::{InternalAuth, ProxyTunnelToolSet};
use super::registrations::{TunnelRegistrationRow, TunnelRegistrations, TUNNEL_NOTIFY_CHANNEL};
use super::TunnelRegistry;

pub struct ReconcileCtx<'a> {
    pub regs: &'a TunnelRegistrations,
    pub self_pod_addr: Option<&'a str>,
    pub toolsets: &'a ToolSets,
    pub tunnels: &'a TunnelRegistry,
    pub http: &'a Arc<reqwest::Client>,
    pub auth: &'a Arc<InternalAuth>,
}

impl ReconcileCtx<'_> {
    fn install_proxies(&self, row: &TunnelRegistrationRow) {
        let arcs: Vec<Arc<dyn SearchableToolSet>> = ProxyTunnelToolSet::build(
            &row.deployment_id,
            row.session_id,
            &row.owner_pod_addr,
            &row.toolsets,
            self.http.clone(),
            self.auth.clone(),
        )
        .into_iter()
        .map(|p| Arc::new(p) as Arc<dyn SearchableToolSet>)
        .collect();
        self.toolsets
            .install_proxy_toolsets(&row.deployment_id, arcs);
    }

    async fn evict_displaced_local(&self, row: &TunnelRegistrationRow, when: &str) -> bool {
        if self
            .tunnels
            .evict_if_session_differs(&row.deployment_id, row.session_id)
            .await
        {
            tracing::info!(
                deployment_id = %row.deployment_id,
                new_owner = %row.owner_pod_addr,
                "evicted local Local {when}; deferring Proxy install"
            );
            true
        } else {
            false
        }
    }
}

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
        let ctx = ReconcileCtx {
            regs: &regs,
            self_pod_addr: self_pod_addr.as_deref(),
            toolsets: &toolsets,
            tunnels: &tunnels,
            http: &http,
            auth: &auth,
        };
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

            if let Err(e) = reconcile_all(&ctx).await {
                tracing::warn!(error = %e, "tunnel listener: initial sweep failed");
            }

            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        let deployment_id = notification.payload();
                        if let Err(e) = reconcile_one(&ctx, deployment_id).await {
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

pub async fn reconcile_one(ctx: &ReconcileCtx<'_>, deployment_id: &str) -> Result<(), sqlx::Error> {
    match ctx.regs.fetch_active(deployment_id).await? {
        None => ctx.toolsets.remove_proxy_toolsets(deployment_id),
        Some(row) if Some(row.owner_pod_addr.as_str()) == ctx.self_pod_addr => {}
        Some(row) => {
            if !ctx.evict_displaced_local(&row, "on takeover").await {
                ctx.install_proxies(&row);
            }
        }
    }
    Ok(())
}

pub async fn reconcile_all(ctx: &ReconcileCtx<'_>) -> Result<(), sqlx::Error> {
    let rows = ctx.regs.fetch_all_active().await?;
    let active_ids: std::collections::HashSet<String> =
        rows.iter().map(|r| r.deployment_id.clone()).collect();

    for stale in ctx.toolsets.proxy_deployment_ids().difference(&active_ids) {
        ctx.toolsets.remove_proxy_toolsets(stale);
    }

    for row in rows {
        if Some(row.owner_pod_addr.as_str()) == ctx.self_pod_addr {
            continue;
        }
        if !ctx
            .evict_displaced_local(&row, "during the full sweep")
            .await
        {
            ctx.install_proxies(&row);
        }
    }
    Ok(())
}
