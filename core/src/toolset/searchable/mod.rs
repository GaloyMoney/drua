//! [`SearchableToolSet`](super::traits::SearchableToolSet) implementations:
//! upstream MCP servers, the built-in Concourse client, and the
//! code-assistant search toolset. Re-exported from `toolset::` for
//! convenience.

pub mod code_assistant;
pub mod concourse;
mod jwt_http_client;
pub mod remote_proxy;
pub mod upstream;

pub use code_assistant::CodeAssistantToolSet;
pub use concourse::ConcourseToolSet;
pub use remote_proxy::RemoteProxyToolSet;
pub use upstream::*;
