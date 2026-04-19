//! [`SearchableToolSet`](super::traits::SearchableToolSet) implementations:
//! upstream MCP servers, the built-in Concourse client, the code-assistant
//! search toolset, and the admin tool adapter. Re-exported from `toolset::`
//! for convenience.

pub mod admin;
pub mod code_assistant;
pub mod concourse;
pub mod upstream;

pub use admin::AdminToolSet;
pub use code_assistant::CodeAssistantToolSet;
pub use concourse::ConcourseToolSet;
pub use upstream::*;
