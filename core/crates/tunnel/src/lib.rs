mod config;
mod listener;
mod protocol;
mod registrations;
mod registry;

pub use config::{
    default_tunnel_expires_after_secs, default_tunnel_heartbeat_secs,
    default_tunnel_reaper_interval_secs, TunnelConfigError, TunnelRuntimeConfig,
};
pub use listener::{
    reconcile_all, reconcile_one, spawn_registry_listener, ReconcileCtx, ReconcileTarget,
};
pub use protocol::{
    InternalAuth, InternalCallReq, RegisteredToolSet, TunnelError, TunnelMessage,
    TUNNEL_PROXY_CALL_TIMEOUT, TUNNEL_PROXY_TIMEOUT_SLACK, TUNNEL_PROXY_TIMEOUT_SLACK_SECS,
    TUNNEL_TOOL_CALL_TIMEOUT, TUNNEL_TOOL_CALL_TIMEOUT_SECS,
};
pub use registrations::{TunnelRegistrationRow, TunnelRegistrations, TUNNEL_NOTIFY_CHANNEL};
pub use registry::{TunnelHandle, TunnelRegistry};

pub mod wire {
    pub use rmcp::model::{CallToolResult, JsonObject};
}
