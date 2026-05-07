//! drua-git-proxy: leaf-library primitives for the smart-HTTP git proxy.
//!
//! No `drua-core` dependency — this crate holds the pure git-server
//! protocol types, the YAML-driven allow-list, and the policy
//! evaluator. Wire-protocol handling (`git http-backend` spawn,
//! per-project mirror, pkt-line peek, upstream push) lands in the
//! same crate as it's added (M1.5+).

pub mod allowlist;
pub mod error;
pub mod policy;
pub mod primitives;

pub use allowlist::{Allowlist, AllowlistConfig, AllowlistEntry, AllowlistEntryConfig};
pub use error::AllowlistError;
pub use policy::{mode_allowed, RefPatternSet};
pub use primitives::{GitProxyDecision, GitProxyMode, GitService, RepoCoord};
