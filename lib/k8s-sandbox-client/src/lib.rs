//! Kubernetes client for the Agent Sandbox controller (agents.x-k8s.io).
//!
//! Provides a high-level API to manage sandbox lifecycle via direct Sandbox
//! resources with persistent volumes for session history.

mod client;
mod error;
mod types;

pub use client::{PersistenceConfig, ResourceConfig, SandboxClient, SandboxSummary};
pub use error::SandboxError;
