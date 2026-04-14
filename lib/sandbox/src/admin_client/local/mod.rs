mod config;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::instrument;

pub use self::config::LocalSandboxConfig;
use crate::admin_client::AdminClient;
use crate::error::AdminError;
use crate::types::{Sandbox as SandboxView, SandboxSpecs};

const SANDBOXES_DIR_NAME: &str = ".sandboxes";
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// A running sandbox process owned by the client. The child is killed on drop.
struct RunningSandbox {
    child: Child,
    view: SandboxView,
}

/// Spawns and tracks sandbox tool server processes locally.
///
/// Each sandbox lives in `<repo_root>/.sandboxes/<id>/` with a `workspace/`
/// subdir mounted as `WORKSPACE_ROOT` and a `secrets/github-token` path the
/// `/initialize` endpoint can write to.
#[derive(Clone)]
pub struct LocalAdminClient {
    config: LocalSandboxConfig,
    sandboxes_root: PathBuf,
    ready_timeout: Duration,
    sandboxes: Arc<Mutex<HashMap<String, RunningSandbox>>>,
}

impl LocalAdminClient {
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

    #[instrument(
        name = "sandbox.admin.local.create_sandbox",
        skip(self, specs),
        fields(%name, cpu = %specs.cpu, memory = %specs.memory, disk_size = %specs.disk_size)
    )]
    pub async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<SandboxView, AdminError> {
        // Specs are recorded in the span fields above for visibility but
        // are not enforced by the local backend.
        let _ = specs;
        {
            let sandboxes = self.sandboxes.lock().await;
            if sandboxes.contains_key(name) {
                return Err(AdminError::AlreadyExists(name.to_string()));
            }
        }

        let sandbox_dir = self.sandboxes_root.join(name);
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

        let child = cmd.spawn().map_err(AdminError::Spawn)?;

        wait_ready(port, self.ready_timeout)
            .await
            .map_err(|_| AdminError::Timeout(name.to_string()))?;

        let view = SandboxView {
            name: name.to_string(),
            phase: "Ready".to_string(),
            ready: true,
            base_url: Some(base_url),
            service_fqdn: None,
        };

        let mut sandboxes = self.sandboxes.lock().await;
        sandboxes.insert(
            name.to_string(),
            RunningSandbox {
                child,
                view: view.clone(),
            },
        );

        tracing::info!(sandbox = %name, port, "Local sandbox spawned");
        Ok(view)
    }

    #[instrument(name = "sandbox.admin.local.delete_sandbox", skip(self), fields(%name))]
    pub async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError> {
        let mut sandboxes = self.sandboxes.lock().await;
        let mut sb = sandboxes
            .remove(name)
            .ok_or_else(|| AdminError::NotFound(name.to_string()))?;
        // Best-effort: ignore errors if the child already exited.
        let _ = sb.child.kill().await;
        tracing::info!(sandbox = %name, "Local sandbox stopped");
        Ok(())
    }

    pub async fn get_sandbox(&self, name: &str) -> Result<SandboxView, AdminError> {
        let sandboxes = self.sandboxes.lock().await;
        sandboxes
            .get(name)
            .map(|s| s.view.clone())
            .ok_or_else(|| AdminError::NotFound(name.to_string()))
    }

    pub async fn list_sandboxes(&self) -> Vec<SandboxView> {
        let sandboxes = self.sandboxes.lock().await;
        sandboxes.values().map(|s| s.view.clone()).collect()
    }
}

#[async_trait]
impl AdminClient for LocalAdminClient {
    async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<SandboxView, AdminError> {
        LocalAdminClient::create_sandbox(self, name, specs).await
    }

    async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError> {
        LocalAdminClient::delete_sandbox(self, name).await
    }

    async fn get_sandbox(&self, name: &str) -> Result<SandboxView, AdminError> {
        LocalAdminClient::get_sandbox(self, name).await
    }

    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, AdminError> {
        Ok(LocalAdminClient::list_sandboxes(self).await)
    }

    /// For the local backend `create_sandbox` already blocks until ready, so
    /// this is effectively a no-op lookup.
    async fn wait_sandbox_ready(
        &self,
        name: &str,
        _timeout: Duration,
    ) -> Result<SandboxView, AdminError> {
        self.get_sandbox(name).await
    }
}

/// Allocate a free TCP port by binding to port 0. There's a small race window
/// between drop and the spawned child binding, but it's acceptable for local
/// dev/test usage.
fn allocate_port() -> Result<u16, AdminError> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(AdminError::PortAllocation)?;
    let port = listener
        .local_addr()
        .map_err(AdminError::PortAllocation)?
        .port();
    Ok(port)
}

async fn create_dir_all(path: &Path) -> Result<(), AdminError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|source| AdminError::Io {
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

    fn test_specs() -> SandboxSpecs {
        SandboxSpecs {
            cpu: "100m".into(),
            memory: "128Mi".into(),
            disk_size: "1Gi".into(),
        }
    }

    #[tokio::test]
    async fn allocate_port_returns_distinct_ports() {
        let p1 = allocate_port().unwrap();
        let p2 = allocate_port().unwrap();
        assert_ne!(p1, 0);
        assert_ne!(p1, p2);
    }

    #[tokio::test]
    async fn sandboxes_root_is_under_repo_root() {
        let tmp = std::env::temp_dir().join("sandbox-test-root");
        let client = LocalAdminClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "true".into(),
            },
            &tmp,
        );
        assert_eq!(client.sandboxes_root(), tmp.join(".sandboxes"));
    }

    #[tokio::test]
    async fn create_sandbox_creates_workspace_and_secrets_dirs() {
        let tmp = std::env::temp_dir().join("sandbox-test-dirs");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let client = LocalAdminClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "true".into(),
            },
            &tmp,
        )
        .with_ready_timeout(Duration::from_millis(200));

        // `true` exits immediately without binding, so wait_ready times out.
        let err = client
            .create_sandbox("alpha", &test_specs())
            .await
            .expect_err("should time out — `true` never binds a port");
        assert!(matches!(err, AdminError::Timeout(_)));

        // The dirs should still have been created before the spawn attempt.
        let workspace = tmp.join(".sandboxes/alpha/workspace");
        let secrets = tmp.join(".sandboxes/alpha/secrets");
        assert!(workspace.is_dir(), "workspace dir not created");
        assert!(secrets.is_dir(), "secrets dir not created");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn create_sandbox_rejects_duplicate_id() {
        let tmp = std::env::temp_dir().join("sandbox-test-dup");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let client = LocalAdminClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "sleep 60".into(),
            },
            &tmp,
        )
        .with_ready_timeout(Duration::from_millis(100));

        // First call times out (sleep doesn't bind), but we manually inject the
        // sandbox into the map to simulate one already running.
        let _ = client.create_sandbox("dup", &test_specs()).await;
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
                    view: SandboxView {
                        name: "dup".into(),
                        phase: "Ready".into(),
                        ready: true,
                        base_url: Some("http://127.0.0.1:1".into()),
                        service_fqdn: None,
                    },
                },
            );
        }

        let err = client
            .create_sandbox("dup", &test_specs())
            .await
            .expect_err("duplicate");
        assert!(matches!(err, AdminError::AlreadyExists(_)));

        let _ = client.delete_sandbox("dup").await;
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn delete_unknown_sandbox_returns_not_found() {
        let tmp = std::env::temp_dir().join("sandbox-test-unknown");
        let client = LocalAdminClient::new(
            LocalSandboxConfig {
                sandbox_spawn_cmd: "true".into(),
            },
            &tmp,
        );
        let err = client
            .delete_sandbox("missing")
            .await
            .expect_err("not found");
        assert!(matches!(err, AdminError::NotFound(_)));
    }
}
