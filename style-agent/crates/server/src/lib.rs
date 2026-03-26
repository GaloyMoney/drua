mod config;
mod endpoints;
mod request_logger;
mod standalone_server;

pub use config::StyleAgentConfig;
pub use endpoints::init_endpoints;
pub use endpoints::init_endpoints_with_logger;
pub use endpoints::{SearchCodeParams, StyleAgentEndpoints};
pub use request_logger::{LoggerError, NoopRequestLogger, RequestLogger};
pub use standalone_server::{
    init_router, router, run_server, run_server_with_config, StyleAgentServer,
};

pub use style_agent_core::request_log;
pub use style_agent_core::search::SearchEngine;
pub use style_agent_core::store::SearchResult;
