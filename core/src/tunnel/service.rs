use std::collections::HashSet;
use std::sync::Arc;

use drua_tunnel::{
    InternalAuth, ReconcileCtx, ReconcileTarget, TunnelConfigError, TunnelRegistrationRow,
    TunnelRegistrations, TunnelRegistry, TunnelRuntimeConfig,
};

use crate::toolset::{SearchableToolSet, ToolSets};

use super::ProxyTunnelToolSet;

#[derive(thiserror::Error, Debug)]
pub enum TunnelServiceError {
    #[error("config: {0}")]
    Config(#[from] TunnelConfigError),
    #[error("http client: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct TunnelService {
    registrations: Arc<TunnelRegistrations>,
    registry: Arc<TunnelRegistry>,
    runtime: TunnelRuntimeConfig,
    target: Arc<CoreReconcileTarget>,
}

impl TunnelService {
    pub fn start(
        pool: sqlx::PgPool,
        runtime: TunnelRuntimeConfig,
        toolsets: Arc<ToolSets>,
    ) -> Result<Self, TunnelServiceError> {
        runtime.validate()?;

        let registrations = Arc::new(TunnelRegistrations::new(pool.clone()));
        spawn_reaper(
            Arc::clone(&registrations),
            std::time::Duration::from_secs(runtime.reaper_interval_secs.max(1)),
        );

        let http = Arc::new(reqwest::Client::builder().build()?);
        let auth = Arc::new(auth_from_runtime(&runtime));
        let registry = Arc::new(TunnelRegistry::new());
        let mut runtime = runtime;

        if runtime.self_pod_addr.is_some() && matches!(*auth, InternalAuth::Disabled) {
            tracing::warn!(
                "POD_IP is set but no tunnel internal_secret configured - \
                 falling back to single-replica tunnel mode. Set \
                 DRUA_TUNNEL_INTERNAL_SECRET (Helm secrets.tunnelInternalSecret) \
                 to enable cross-pod tunnel HA."
            );
            runtime.self_pod_addr = None;
        }

        let target = Arc::new(CoreReconcileTarget::new(
            toolsets,
            Arc::clone(&registry),
            http,
            auth,
        ));

        if runtime.self_pod_addr.is_some() {
            drua_tunnel::spawn_registry_listener(
                pool,
                runtime.self_pod_addr.clone(),
                Arc::clone(&target),
            );
        }

        Ok(Self {
            registrations,
            registry,
            runtime,
            target,
        })
    }

    pub fn registry(&self) -> &TunnelRegistry {
        &self.registry
    }

    pub fn registrations(&self) -> &TunnelRegistrations {
        &self.registrations
    }

    pub fn runtime(&self) -> &TunnelRuntimeConfig {
        &self.runtime
    }

    pub async fn reconcile(&self, deployment_id: &str) -> Result<(), sqlx::Error> {
        let ctx = ReconcileCtx {
            regs: &self.registrations,
            self_pod_addr: self.runtime.self_pod_addr.as_deref(),
            target: self.target.as_ref(),
        };
        drua_tunnel::reconcile_one(&ctx, deployment_id).await
    }

    pub async fn shutdown(&self) {
        let Some(addr) = self.runtime.self_pod_addr.as_deref() else {
            return;
        };
        match self.registrations.delete_all_owned_by(addr).await {
            Ok(n) if n > 0 => tracing::info!(count = n, owner = %addr, "tunnel rows handed off"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "tunnel shutdown cleanup failed"),
        }
    }
}

pub struct CoreReconcileTarget {
    toolsets: Arc<ToolSets>,
    registry: Arc<TunnelRegistry>,
    http: Arc<reqwest::Client>,
    auth: Arc<InternalAuth>,
}

impl CoreReconcileTarget {
    pub fn new(
        toolsets: Arc<ToolSets>,
        registry: Arc<TunnelRegistry>,
        http: Arc<reqwest::Client>,
        auth: Arc<InternalAuth>,
    ) -> Self {
        Self {
            toolsets,
            registry,
            http,
            auth,
        }
    }
}

#[async_trait::async_trait]
impl ReconcileTarget for CoreReconcileTarget {
    fn install_proxy_toolsets(&self, row: &TunnelRegistrationRow) {
        let proxies: Vec<Arc<dyn SearchableToolSet>> = ProxyTunnelToolSet::build(
            &row.deployment_id,
            row.session_id,
            &row.owner_pod_addr,
            &row.toolsets,
            Arc::clone(&self.http),
            Arc::clone(&self.auth),
        )
        .into_iter()
        .map(|ts| Arc::new(ts) as Arc<dyn SearchableToolSet>)
        .collect();

        self.toolsets
            .install_proxy_toolsets(&row.deployment_id, proxies);
    }

    fn remove_proxy_toolsets(&self, deployment_id: &str) {
        self.toolsets.remove_proxy_toolsets(deployment_id);
    }

    fn proxy_deployment_ids(&self) -> HashSet<String> {
        self.toolsets.proxy_deployment_ids()
    }

    async fn evict_displaced_local(&self, row: &TunnelRegistrationRow, when: &str) -> bool {
        let evicted = self
            .registry
            .evict_if_session_differs(&row.deployment_id, row.session_id)
            .await;
        if evicted {
            tracing::info!(
                deployment_id = %row.deployment_id,
                owner = %row.owner_pod_addr,
                session_id = %row.session_id,
                %when,
                "local tunnel displaced by peer registry row"
            );
        }
        evicted
    }
}

fn spawn_reaper(regs: Arc<TunnelRegistrations>, interval: std::time::Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            match regs.reap_expired().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "tunnel reaper deleted expired rows"),
                Err(e) => tracing::warn!(error = %e, "tunnel reaper failed"),
            }
        }
    });
}

fn auth_from_runtime(runtime: &TunnelRuntimeConfig) -> InternalAuth {
    match runtime.internal_secret.as_deref() {
        Some(s) if !s.is_empty() => InternalAuth::SharedSecret {
            secret: s.to_string(),
        },
        _ => InternalAuth::Disabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_disabled_when_no_secret() {
        let runtime = TunnelRuntimeConfig {
            internal_secret: None,
            ..Default::default()
        };
        assert!(matches!(
            auth_from_runtime(&runtime),
            InternalAuth::Disabled
        ));
    }

    #[test]
    fn auth_disabled_on_empty_secret() {
        let runtime = TunnelRuntimeConfig {
            internal_secret: Some(String::new()),
            ..Default::default()
        };
        assert!(matches!(
            auth_from_runtime(&runtime),
            InternalAuth::Disabled
        ));
    }

    #[test]
    fn auth_shared_secret_when_set() {
        let runtime = TunnelRuntimeConfig {
            internal_secret: Some("hunter2".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            auth_from_runtime(&runtime),
            InternalAuth::SharedSecret { .. }
        ));
    }
}
