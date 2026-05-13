mod owned;
mod proxy;
mod service;

pub use drua_tunnel::{
    reconcile_all as reconcile_all_deployments, reconcile_one as reconcile_one_deployment,
    InternalAuth, InternalCallReq, ReconcileCtx, ReconcileTarget, RegisteredToolSet, TunnelHandle,
    TunnelMessage, TunnelRegistrationRow, TunnelRegistrations, TunnelRegistry, TunnelRuntimeConfig,
    TUNNEL_PROXY_CALL_TIMEOUT, TUNNEL_PROXY_TIMEOUT_SLACK, TUNNEL_PROXY_TIMEOUT_SLACK_SECS,
    TUNNEL_TOOL_CALL_TIMEOUT, TUNNEL_TOOL_CALL_TIMEOUT_SECS,
};
pub use owned::OwnedTunnelToolSet;
pub use proxy::ProxyTunnelToolSet;
pub use service::{CoreReconcileTarget, TunnelService, TunnelServiceError};

pub mod wire {
    pub use drua_tunnel::wire::{CallToolResult, JsonObject};
}
