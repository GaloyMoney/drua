use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::instrument;

use crate::config::LocalSandboxConfig;
use crate::error::LocalSandboxError;

const SANDBOXES_DIR_NAME: &str = ".sandboxes";
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Public view of a running sandbox.
#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub id: String,
    pub port: u16,
    pub base_url: String,
    pub workspace: PathBuf,
    pub github_token_path: PathBuf,
}

/// A running sandbox process owned by the client. The child is killed on drop.
struct RunningSandbox {
    child: Child,
    info: SandboxInfo,
}

/// Spawns and tracks sandbox tool server processes locally.
///
/// Each sandbox lives in `<repo_root>/.sandboxes/<id>/` with a `workspace/`
/// subdir mounted as `WORKSPACE_ROOT` and a `secrets/github-token` path the
/// `/initialize` endpoint can write to.
#[derive(Clone)]
pub struct LocalSandboxClient {
    config: LocalSandboxConfig,
    sandboxes_root: PathBuf,
    ready_timeout: Duration,
    sandboxes: Arc<Mutex<HashMap<String, RunningSandbox>>>,
}

impl LocalSandboxClient {
    /// Create a new client. Sandboxes will be placed under
    /// `<repo_root>/.sandboxes/`.
    pub fn new(config: LocalSandboxConfig, repo_root: impl AsRef<Path>) -> Self {
        Self {
            config,
            sandboxes_root: repo_root.as_ref().join(SANDBOXES_DIR_NAME),
            ready_timeout: READY_TIMEOUT,
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Override how long [`Self::create_sandbox`] waits for the spawned
    /// process to start accepting TCP connections (default: 30s).
    pub fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// Root directory where per-sandbox state is kept.
    pub fn sandboxes_root(&self) -> &Path {
        &self.sandboxes_root
    }

    #[instrument(name = "local_sandbox_client.create_sandbox", skip(self), fields(%id))]
    pub async fn create_sandbox(&self, id: &str) -> Result<SandboxInfo, LocalSandboxError> {
        {
            let sandboxes = self.sandboxes.lock().await;
            if sandboxes.contains_key(id) {
                return Err(LocalSandboxError::AlreadyExists(id.to_string()));
            }
        }

        let sandbox_dir = self.sandboxes_root.join(id);
        let workspace = sandbox_dir.join("workspace");
        let secrets_dir = sandbox_dir.join("secrets");
        let github_token_path = secrets_dir.join("github-token");

        create_dir_all(&workspace).await?;
        create_dir_all(&secrets_dir).await?;

        let port = allocate_port()?;
        let base_url = format!("http://127.0.0.1:{port}");

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&self.config.sandbox_spawn_cmd)
            .env("PORT", port.to_string())
            .env("WORKSPACE_ROOT", &workspace)
            .env("GITHUB_TOKEN_PATH", &github_token_path)
            .current_dir(&sandbox_dir)
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(LocalSandboxError::Spawn)?;

        let info = SandboxInfo {
            id: id.to_string(),
            port,
            base_url,
            workspace,
            github_token_path,
        };

        wait_ready(port, self.ready_timeout).await.map_err(|_| {
            // Best-effort cleanup is via Drop on `child`, which is currently owned.
            LocalSandboxError::Timeout(id.to_string())
        })?;

        let mut sandboxes = self.sandboxes.lock().await;
        sandboxes.insert(
            id.to_string(),
            RunningSandbox {
                child,
                info: info.clone(),
            },
        );

        tracing::info!(sandbox = %id, port, "Local sandbox spawned");
        Ok(info)
    }

    #[instrument(name = "local_sandbox_client.delete_sandbox", skip(self), fields(%id))]
    pub async fn delete_sandbox(&self, id: &str) -> Result<(), LocalSandboxError> {
        let mut sandboxes = self.sandboxes.lock().await;
        let mut sb = sandboxes
            .remove(id)
            .ok_or_else(|| LocalSandboxError::NotFound(id.to_string()))?;
        // Best-effort: ignore errors if the child already exited.
        let _ = sb.child.kill().await;
        tracing::info!(sandbox = %id, "Local sandbox stopped");
        Ok(())
    }

    pub async fn get_sandbox(&self, id: &str) -> Result<SandboxInfo, LocalSandboxError> {
        let sandboxes = self.sandboxes.lock().await;
        sandboxes
            .get(id)
            .map(|s| s.info.clone())
            .ok_or_else(|| LocalSandboxError::NotFound(id.to_string()))
    }

    pub async fn list_sandboxes(&self) -> Vec<SandboxInfo> {
        let sandboxes = self.sandboxes.lock().await;
        sandboxes.values().map(|s| s.info.clone()).collect()
    }
}

/// Allocate a free TCP port by binding to port 0. There's a small race window
/// between drop and the spawned child binding, but it's acceptable for local
/// dev/test usage.
fn allocate_port() -> Result<u16, LocalSandboxError> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(LocalSandboxError::PortAllocation)?;
    let port = listener
        .local_addr()
        .map_err(LocalSandboxError::PortAllocation)?
        .port();
    Ok(port)
}

async fn create_dir_all(path: &Path) -> Result<(), LocalSandboxError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|source| LocalSandboxError::Io {
            path: path.display().to_string(),
            source,
        })
}

/// Poll the port until it accepts a TCP connection or the timeout elapses.
async fn wait_ready(port: u16, timeout: Duration) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(());
        }
        sleep(READY_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allocate_port_returns_distinct_ports() {
        let p1 = allocate_port().unwrap();
        let p2 = allocate_port().unwrap();
        assert_ne!(p1, 0);
        assert_ne!(p1, p2);
    }

    #[tokio::test]
    async fn sandboxes_root_is_under_repo_root() {
        let tmp = std::env::temp_dir().join("local-sandbox-client-test-root");
        let client = LocalSandboxClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "true".into(),
            },
            &tmp,
        );
        assert_eq!(client.sandboxes_root(), tmp.join(".sandboxes"));
    }

    #[tokio::test]
    async fn create_sandbox_creates_workspace_and_secrets_dirs() {
        let tmp = std::env::temp_dir().join("local-sandbox-client-test-dirs");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let client = LocalSandboxClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "true".into(),
            },
            &tmp,
        )
        .with_ready_timeout(Duration::from_millis(200));

        // `true` exits immediately without binding, so wait_ready times out.
        let err = client
            .create_sandbox("alpha")
            .await
            .expect_err("should time out — `true` never binds a port");
        assert!(matches!(err, LocalSandboxError::Timeout(_)));

        // The dirs should still have been created before the spawn attempt.
        let workspace = tmp.join(".sandboxes/alpha/workspace");
        let secrets = tmp.join(".sandboxes/alpha/secrets");
        assert!(workspace.is_dir(), "workspace dir not created");
        assert!(secrets.is_dir(), "secrets dir not created");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn create_sandbox_rejects_duplicate_id() {
        let tmp = std::env::temp_dir().join("local-sandbox-client-test-dup");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let client = LocalSandboxClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "sleep 60".into(),
            },
            &tmp,
        )
        .with_ready_timeout(Duration::from_millis(100));

        // First call times out (sleep doesn't bind), but we manually inject the
        // sandbox into the map to simulate one already running.
        let _ = client.create_sandbox("dup").await;
        {
            let mut map = client.sandboxes.lock().await;
            map.insert(
                "dup".to_string(),
                RunningSandbox {
                    child: Command::new("sleep")
                        .arg("60")
                        .kill_on_drop(true)
                        .spawn()
                        .unwrap(),
                    info: SandboxInfo {
                        id: "dup".into(),
                        port: 1,
                        base_url: "http://127.0.0.1:1".into(),
                        workspace: tmp.join(".sandboxes/dup/workspace"),
                        github_token_path: tmp.join(".sandboxes/dup/secrets/github-token"),
                    },
                },
            );
        }

        let err = client.create_sandbox("dup").await.expect_err("duplicate");
        assert!(matches!(err, LocalSandboxError::AlreadyExists(_)));

        let _ = client.delete_sandbox("dup").await;
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn delete_unknown_sandbox_returns_not_found() {
        let tmp = std::env::temp_dir().join("local-sandbox-client-test-unknown");
        let client = LocalSandboxClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "true".into(),
            },
            &tmp,
        );
        let err = client
            .delete_sandbox("missing")
            .await
            .expect_err("not found");
        assert!(matches!(err, LocalSandboxError::NotFound(_)));
    }
}
