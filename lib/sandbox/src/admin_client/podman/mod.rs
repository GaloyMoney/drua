mod config;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::instrument;

pub use self::config::PodmanSandboxConfig;
use crate::admin_client::AdminClient;
use crate::error::AdminError;
use crate::types::{Sandbox as SandboxView, SandboxSpecs};

const SANDBOXES_DIR_NAME: &str = ".sandboxes";
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const CONTAINER_PORT: u16 = 3000;
const LABEL_MANAGED_BY: &str = "managed-by=galoy-agents";
const POD_NAME_PREFIX: &str = "galoy-sb";

/// A running podman pod tracked by the client.
struct RunningPod {
    _host_port: u16,
    view: SandboxView,
}

/// Manages sandbox lifecycle using Podman pods.
///
/// Each sandbox is a pod named `galoy-sb-<name>` containing (initially) a
/// single container running the sandbox-tool-server image.  A host port is
/// mapped to container port 3000.  Workspace and secrets directories are
/// bind-mounted from `<sandboxes_root>/<name>/`.
///
/// The pod architecture makes it straightforward to add sidecar containers
/// (e.g. postgres) later — they share the pod's network namespace and are
/// reachable at `localhost` from the tool server.
#[derive(Clone)]
pub struct PodmanAdminClient {
    config: PodmanSandboxConfig,
    sandboxes_root: PathBuf,
    ready_timeout: Duration,
    pods: Arc<Mutex<HashMap<String, RunningPod>>>,
}

impl PodmanAdminClient {
    pub fn new(config: PodmanSandboxConfig, repo_root: impl AsRef<Path>) -> Self {
        Self {
            config,
            sandboxes_root: repo_root.as_ref().join(SANDBOXES_DIR_NAME),
            ready_timeout: READY_TIMEOUT,
            pods: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    pub fn sandboxes_root(&self) -> &Path {
        &self.sandboxes_root
    }

    fn pod_name(sandbox_name: &str) -> String {
        format!("{POD_NAME_PREFIX}-{sandbox_name}")
    }

    fn container_name(sandbox_name: &str) -> String {
        format!("{POD_NAME_PREFIX}-{sandbox_name}-sandbox")
    }

    /// Run a podman command and return stdout on success.
    async fn run_podman(&self, args: &[&str]) -> Result<String, AdminError> {
        let output = Command::new(&self.config.podman_bin)
            .args(args)
            .output()
            .await
            .map_err(|e| AdminError::Podman(format!("failed to exec podman: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let cmd = args.first().unwrap_or(&"");
            return Err(AdminError::Podman(format!("podman {cmd} failed: {stderr}")));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[instrument(
        name = "sandbox.admin.podman.create_sandbox",
        skip(self, specs),
        fields(%name, cpu = %specs.cpu, memory = %specs.memory, disk_size = %specs.disk_size)
    )]
    pub async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<SandboxView, AdminError> {
        let _ = specs;
        {
            let pods = self.pods.lock().await;
            if pods.contains_key(name) {
                return Err(AdminError::AlreadyExists(name.to_string()));
            }
        }

        let sandbox_dir = self.sandboxes_root.join(name);
        let workspace = sandbox_dir.join("workspace");
        let secrets_dir = sandbox_dir.join("secrets");

        create_dir_all(&workspace).await?;
        create_dir_all(&secrets_dir).await?;

        let port = allocate_port()?;
        let pod_name = Self::pod_name(name);
        let container_name = Self::container_name(name);
        let base_url = format!("http://127.0.0.1:{port}");

        let port_mapping = format!("{port}:{CONTAINER_PORT}");
        self.run_podman(&[
            "pod",
            "create",
            "--name",
            &pod_name,
            "--label",
            LABEL_MANAGED_BY,
            "-p",
            &port_mapping,
        ])
        .await?;

        let workspace_mount = format!("{}:/workspace", workspace.display());
        let secrets_mount = format!("{}:/run/secrets", secrets_dir.display());
        let result = self
            .run_podman(&[
                "run",
                "-d",
                "--pod",
                &pod_name,
                "--name",
                &container_name,
                "-v",
                &workspace_mount,
                "-v",
                &secrets_mount,
                "--label",
                LABEL_MANAGED_BY,
                &self.config.image,
            ])
            .await;

        if let Err(e) = result {
            // Best-effort cleanup of the pod we just created.
            let _ = self.run_podman(&["pod", "rm", "-f", &pod_name]).await;
            return Err(e);
        }

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

        let mut pods = self.pods.lock().await;
        pods.insert(
            name.to_string(),
            RunningPod {
                _host_port: port,
                view: view.clone(),
            },
        );

        tracing::info!(sandbox = %name, port, pod = %pod_name, "Podman sandbox created");
        Ok(view)
    }

    #[instrument(name = "sandbox.admin.podman.delete_sandbox", skip(self), fields(%name))]
    pub async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError> {
        let mut pods = self.pods.lock().await;
        pods.remove(name)
            .ok_or_else(|| AdminError::NotFound(name.to_string()))?;

        let pod_name = Self::pod_name(name);
        // Force-remove the pod and all its containers.
        let _ = self.run_podman(&["pod", "rm", "-f", &pod_name]).await;
        tracing::info!(sandbox = %name, pod = %pod_name, "Podman sandbox deleted");
        Ok(())
    }

    pub async fn get_sandbox(&self, name: &str) -> Result<SandboxView, AdminError> {
        let pods = self.pods.lock().await;
        pods.get(name)
            .map(|p| p.view.clone())
            .ok_or_else(|| AdminError::NotFound(name.to_string()))
    }

    pub async fn list_sandboxes(&self) -> Vec<SandboxView> {
        let pods = self.pods.lock().await;
        pods.values().map(|p| p.view.clone()).collect()
    }
}

#[async_trait]
impl AdminClient for PodmanAdminClient {
    async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<SandboxView, AdminError> {
        PodmanAdminClient::create_sandbox(self, name, specs).await
    }

    async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError> {
        PodmanAdminClient::delete_sandbox(self, name).await
    }

    async fn get_sandbox(&self, name: &str) -> Result<SandboxView, AdminError> {
        PodmanAdminClient::get_sandbox(self, name).await
    }

    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, AdminError> {
        Ok(PodmanAdminClient::list_sandboxes(self).await)
    }

    /// `create_sandbox` already blocks until ready, so this is a lookup.
    async fn wait_sandbox_ready(
        &self,
        name: &str,
        _timeout: Duration,
    ) -> Result<SandboxView, AdminError> {
        self.get_sandbox(name).await
    }
}

/// Allocate a free TCP port by binding to port 0.
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

    #[tokio::test]
    async fn allocate_port_returns_distinct_ports() {
        let p1 = allocate_port().unwrap();
        let p2 = allocate_port().unwrap();
        assert_ne!(p1, 0);
        assert_ne!(p1, p2);
    }

    #[tokio::test]
    async fn sandboxes_root_is_under_repo_root() {
        let tmp = std::env::temp_dir().join("podman-sandbox-test-root");
        let client = PodmanAdminClient::new(
            PodmanSandboxConfig {
                image: "test:latest".into(),
                podman_bin: "podman".into(),
            },
            &tmp,
        );
        assert_eq!(client.sandboxes_root(), tmp.join(".sandboxes"));
    }

    #[tokio::test]
    async fn pod_and_container_names() {
        assert_eq!(PodmanAdminClient::pod_name("abc"), "galoy-sb-abc");
        assert_eq!(
            PodmanAdminClient::container_name("abc"),
            "galoy-sb-abc-sandbox"
        );
    }

    #[tokio::test]
    async fn get_unknown_sandbox_returns_not_found() {
        let tmp = std::env::temp_dir().join("podman-sandbox-test-unknown");
        let client = PodmanAdminClient::new(
            PodmanSandboxConfig {
                image: "test:latest".into(),
                podman_bin: "podman".into(),
            },
            &tmp,
        );
        let err = client.get_sandbox("missing").await.expect_err("not found");
        assert!(matches!(err, AdminError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_unknown_sandbox_returns_not_found() {
        let tmp = std::env::temp_dir().join("podman-sandbox-test-del");
        let client = PodmanAdminClient::new(
            PodmanSandboxConfig {
                image: "test:latest".into(),
                podman_bin: "podman".into(),
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
