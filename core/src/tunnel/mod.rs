mod local;
mod proxy;
mod service;

pub use drua_tunnel::{
    reconcile_all as reconcile_all_deployments, reconcile_one as reconcile_one_deployment,
    InternalAuth, InternalCallReq, ReconcileCtx, ReconcileTarget, RegisteredToolSet, TunnelHandle,
    TunnelMessage, TunnelRegistrationRow, TunnelRegistrations, TunnelRegistry, TunnelRuntimeConfig,
};
pub use local::LocalTunnelToolSet;
pub use proxy::ProxyTunnelToolSet;
pub use service::{CoreReconcileTarget, TunnelService, TunnelServiceError};

pub mod wire {
    pub use drua_tunnel::wire::{CallToolResult, JsonObject};
}
