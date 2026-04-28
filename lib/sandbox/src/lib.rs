//! Unified sandbox client. [`admin_client`] manages lifecycle via the K8s
//! (`agents.x-k8s.io`) and local-process backends; [`InstanceClient`] is the
//! HTTP wrapper over a single sandbox's `/initialize` and `/execute`.

pub mod admin_client;
pub mod error;
pub mod instance_client;
mod types;

pub use admin_client::AdminClient;
pub use error::{AdminError, InstanceError};
pub use instance_client::InstanceClient;
pub use types::{Sandbox, SandboxMode, SandboxSpecs};
