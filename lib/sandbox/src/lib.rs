//! Unified sandbox client.
//!
//! [`admin_client`] manages sandbox lifecycle via two backends:
//! - [`admin_client::K8sAdminClient`] talks to the Agent Sandbox controller
//!   (`agents.x-k8s.io`) on Kubernetes.
//! - [`admin_client::LocalAdminClient`] spawns the sandbox tool server as a
//!   child process for local development and integration testing.
//!
//! Both implement [`admin_client::AdminClient`] and return the same
//! [`Sandbox`] shape; fields that are k8s-specific (e.g. `service_fqdn`) are
//! `None` for the local backend.
//!
//! [`InstanceClient`] is a thin HTTP wrapper over a single sandbox's
//! `/initialize` and `/execute` endpoints.

pub mod admin_client;
pub mod error;
pub mod instance_client;
mod types;

pub use admin_client::AdminClient;
pub use error::{AdminError, InstanceError};
pub use instance_client::InstanceClient;
pub use types::Sandbox;
