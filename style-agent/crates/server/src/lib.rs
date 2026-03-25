mod config;
mod endpoints;
mod standalone_server;

pub use config::StyleAgentConfig;
pub use endpoints::init_endpoints;
pub use endpoints::{SearchCodeParams, StyleAgentEndpoints};
pub use standalone_server::{
    init_router, router, run_server, run_server_with_config, StyleAgentServer,
};

pub use style_agent_core::search::SearchEngine;
