use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalSandboxConfig {
    /// Shell command (run via `sh -c`) that spawns the sandbox tool server.
    /// The child receives `PORT`, `PROJECT_ROOT`, and `GITHUB_TOKEN_PATH`
    /// in its environment.
    pub sandbox_spawn_cmd: String,
}
