//! drua-git-proxy: leaf-library primitives for the smart-HTTP git proxy.
//!
//! No `drua-core` dependency — pure git-server protocol types, the
//! YAML-driven allow-list, the policy evaluator, the per-project bare
//! mirror manager, and the `git http-backend` CGI wrapper.

pub mod allowlist;
pub mod cgi;
pub mod error;
pub mod mirror;
pub mod policy;
pub mod primitives;

pub use allowlist::{Allowlist, AllowlistConfig, AllowlistEntry, AllowlistEntryConfig};
pub use cgi::{spawn_http_backend, CgiError, CgiRequest, CgiResponse};
pub use error::AllowlistError;
pub use mirror::{
    MirrorConfig, MirrorError, MirrorManager, StaticCredential, UpstreamCredentialProvider,
};
pub use policy::{mode_allowed, RefPatternSet};
pub use primitives::{GitProxyDecision, GitProxyMode, GitService, RepoCoord};
