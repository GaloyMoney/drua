//! Kubernetes client for the Agent Sandbox controller (agents.x-k8s.io).
//!
//! Provides a high-level API to manage sandbox lifecycle via SandboxClaim
//! resources, backed by a warm pool of pre-created gVisor-isolated pods.

mod client;
mod error;
mod types;

pub use client::{SandboxClient, SandboxSummary};
pub use error::SandboxError;
pub use types::*;
