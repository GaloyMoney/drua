//! Local sandbox client.
//!
//! Spawns the sandbox tool server as a child process for development and
//! integration testing, with each sandbox isolated under
//! `<repo_root>/.sandboxes/<id>/`. The exact spawn invocation is provided by
//! the caller via [`LocalSandboxConfig::sandbox_spawn_cmd`] so the same client
//! works whether the binary is run via `cargo run`, `nix run`, or a path to a
//! prebuilt binary.

mod client;
mod config;
mod error;

pub use client::{LocalSandboxClient, SandboxInfo};
pub use config::LocalSandboxConfig;
pub use error::LocalSandboxError;
