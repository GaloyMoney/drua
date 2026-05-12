use std::collections::HashSet;
use std::sync::Arc;

use sqlx::PgPool;

use crate::registrations::{TunnelRegistrationRow, TunnelRegistrations, TUNNEL_NOTIFY_CHANNEL};

#[async_trait::async_trait]
pub trait ReconcileTarget: Send + Sync {
    fn install_proxy_toolsets(&self, row: &TunnelRegistrationRow);
    fn remove_proxy_toolsets(&self, deployment_id: &str);
    fn proxy_deployment_ids(&self) -> HashSet<String>;
    async fn evict_displaced_local(&self, row: &TunnelRegistrationRow, when: &str) -> bool;
}

pub struct ReconcileCtx<'a, T: ReconcileTarget + ?Sized> {
    pub regs: &'a TunnelRegistrations,
    pub self_pod_addr: Option<&'a str>,
    pub target: &'a T,
}

pub fn spawn_registry_listener<T>(pool: PgPool, self_pod_addr: Option<String>, target: Arc<T>)
where
    T: ReconcileTarget + 'static,
{
    tokio::spawn(async move {
        let regs = TunnelRegistrations::new(pool.clone());
        let ctx = ReconcileCtx {
            regs: &regs,
            self_pod_addr: self_pod_addr.as_deref(),
            target: target.as_ref(),
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

pub async fn reconcile_one<T>(
    ctx: &ReconcileCtx<'_, T>,
    deployment_id: &str,
) -> Result<(), sqlx::Error>
where
    T: ReconcileTarget + ?Sized,
{
    match ctx.regs.fetch_active(deployment_id).await? {
        None => ctx.target.remove_proxy_toolsets(deployment_id),
        Some(row) if Some(row.owner_pod_addr.as_str()) == ctx.self_pod_addr => {}
        Some(row) => {
            if !ctx.target.evict_displaced_local(&row, "on takeover").await {
                ctx.target.install_proxy_toolsets(&row);
            }
        }
    }
    Ok(())
}

pub async fn reconcile_all<T>(ctx: &ReconcileCtx<'_, T>) -> Result<(), sqlx::Error>
where
    T: ReconcileTarget + ?Sized,
{
    let rows = ctx.regs.fetch_all_active().await?;
    let active_ids: HashSet<String> = rows.iter().map(|r| r.deployment_id.clone()).collect();

    for stale in ctx.target.proxy_deployment_ids().difference(&active_ids) {
        ctx.target.remove_proxy_toolsets(stale);
    }

    for row in rows {
        if Some(row.owner_pod_addr.as_str()) == ctx.self_pod_addr {
            continue;
        }
        if !ctx
            .target
            .evict_displaced_local(&row, "during the full sweep")
            .await
        {
            ctx.target.install_proxy_toolsets(&row);
        }
    }
    Ok(())
}
