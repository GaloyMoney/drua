use serde::{Deserialize, Serialize};

/// Configuration for [`super::LocalSandboxClient`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalSandboxConfig {
    /// Shell command used to spawn the sandbox tool server.
    ///
    /// The command is executed via `sh -c`, so pipes/args/redirection are
    /// supported. The client sets `PORT`, `WORKSPACE_ROOT`, and
    /// `GITHUB_TOKEN_PATH` in the child's environment — the spawned binary
    /// must read its bind port and workspace from these variables.
    ///
    /// Examples:
    /// - `cargo run -q -p sandbox-tool-server --`
    /// - `nix run .#sandbox-tool-server --`
    /// - `/usr/local/bin/sandbox-tool-server`
    pub sandbox_spawn_cmd: String,
}
