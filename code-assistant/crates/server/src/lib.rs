mod config;
mod endpoints;
mod request_logger;
mod standalone_server;

pub use config::CodeAssistantConfig;
pub use endpoints::init_endpoints;
pub use endpoints::init_endpoints_with_logger;
pub use endpoints::{CodeAssistantEndpoints, SearchCodeParams};
pub use request_logger::{LoggerError, NoopRequestLogger, RequestLogger};
pub use standalone_server::{
    init_router, router, run_server, run_server_with_config, CodeAssistantServer,
};

pub use code_assistant_core::request_log;
pub use code_assistant_core::search::SearchEngine;
pub use code_assistant_core::store::SearchResult;
