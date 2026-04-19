mod types;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource,
    Pod, PodSecurityContext, ResourceRequirements, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, AttachParams, AttachedProcess, DeleteParams, ListParams, PostParams};
use kube::Client;
use tracing::instrument;

use self::types::*;
use crate::admin_client::AdminClient;
use crate::error::AdminError;
use crate::types::{Sandbox as SandboxView, SandboxSpecs};

const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
const MANAGED_BY_VALUE: &str = "drua";

/// Cluster-level storage class config for the workspace PVC. Per-sandbox
/// disk size comes from [`SandboxSpecs::disk_size`].
#[derive(Clone, Debug)]
pub struct PersistenceConfig {
    pub storage_class: String,
    pub mount_path: String,
}

/// Default port the sandbox tool server binds to inside the pod (matches the
/// `ExposedPorts` in `images/sandbox/default.nix`).
const DEFAULT_SANDBOX_PORT: u16 = 3000;

/// High-level client for managing Agent Sandbox resources.
#[derive(Clone)]
pub struct K8sAdminClient {
    client: Client,
    namespace: String,
    template_name: String,
    persistence: Option<PersistenceConfig>,
    extra_env: Vec<(String, String)>,
    sandbox_port: u16,
}

impl K8sAdminClient {
    /// Create a new sandbox client for the given namespace and template.
    pub fn new(client: Client, namespace: String, template_name: String) -> Self {
        Self {
            client,
            namespace,
            template_name,
            persistence: None,
            extra_env: Vec::new(),
            sandbox_port: DEFAULT_SANDBOX_PORT,
        }
    }

    /// Enable persistent storage for sandboxes.
    pub fn with_persistence(mut self, config: PersistenceConfig) -> Self {
        self.persistence = Some(config);
        self
    }

    /// Inject extra environment variables into every sandbox container
    /// created by this client.
    pub fn with_extra_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }

    /// Override the port used to construct `base_url` (default 3000).
    pub fn with_sandbox_port(mut self, port: u16) -> Self {
        self.sandbox_port = port;
        self
    }

    /// The namespace this client manages sandboxes in.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Build a client from in-cluster or kubeconfig environment.
    pub async fn try_from_env(
        namespace: String,
        template_name: String,
    ) -> Result<Self, AdminError> {
        let client = Client::try_default().await?;
        Ok(Self::new(client, namespace, template_name))
    }

    // -----------------------------------------------------------------------
    // Sandbox management
    // -----------------------------------------------------------------------

    /// Read the SandboxTemplate to get the pod template.
    #[instrument(name = "sandbox.admin.k8s.read_template", skip_all)]
    async fn read_template(&self) -> Result<SandboxTemplate, AdminError> {
        let templates: Api<SandboxTemplate> = Api::namespaced(self.client.clone(), &self.namespace);
        templates
            .get(&self.template_name)
            .await
            .map_err(AdminError::Kube)
    }

    /// Ensure a standalone PVC exists for the given sandbox name.
    /// Creates the PVC if it doesn't exist; no-ops if it already does.
    #[instrument(name = "sandbox.admin.k8s.ensure_pvc", skip_all, fields(%name, %disk_size))]
    async fn ensure_pvc(
        &self,
        name: &str,
        persistence: &PersistenceConfig,
        disk_size: &str,
    ) -> Result<String, AdminError> {
        let pvc_name = format!("workspace-{name}");
        let pvcs: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);

        match pvcs.get(&pvc_name).await {
            Ok(_) => {
                tracing::info!(pvc = %pvc_name, "PVC already exists, reusing");
            }
            Err(kube::Error::Api(resp)) if resp.code == 404 => {
                let mut labels = BTreeMap::new();
                labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());
                labels.insert("sandbox-name".to_string(), name.to_string());

                let pvc = PersistentVolumeClaim {
                    metadata: kube::api::ObjectMeta {
                        name: Some(pvc_name.clone()),
                        namespace: Some(self.namespace.clone()),
                        labels: Some(labels),
                        ..Default::default()
                    },
                    spec: Some(PersistentVolumeClaimSpec {
                        access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                        storage_class_name: Some(persistence.storage_class.clone()),
                        resources: Some(VolumeResourceRequirements {
                            requests: Some(BTreeMap::from([(
                                "storage".to_string(),
                                Quantity(disk_size.to_string()),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                };

                pvcs.create(&PostParams::default(), &pvc).await?;
                tracing::info!(pvc = %pvc_name, "PVC created");
            }
            Err(e) => return Err(AdminError::Kube(e)),
        }

        Ok(pvc_name)
    }

    /// Create a Sandbox with persistent storage.
    ///
    /// Creates a standalone PVC (if it doesn't already exist) and mounts it
    /// into the pod. The PVC lifecycle is independent of the Sandbox — it
    /// survives sandbox deletion and is reused on recreation with the same name.
    #[instrument(name = "sandbox.admin.k8s.create_sandbox", skip_all, fields(%name))]
    pub async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<SandboxView, AdminError> {
        let template = self.read_template().await?;
        let mut pod_template = template.spec.pod_template;

        // Inject extra environment variables into the first container
        if !self.extra_env.is_empty() {
            if let Some(ref mut spec) = pod_template.spec {
                if let Some(container) = spec.containers.first_mut() {
                    let env = container.env.get_or_insert_with(Vec::new);
                    for (k, v) in &self.extra_env {
                        env.push(EnvVar {
                            name: k.clone(),
                            value: Some(v.clone()),
                            value_from: None,
                        });
                    }
                }
            }
        }

        if let Some(ref persistence) = self.persistence {
            let pvc_name = self.ensure_pvc(name, persistence, &specs.disk_size).await?;

            if let Some(ref mut spec) = pod_template.spec {
                // Add PVC as a volume
                let mut volumes = spec.volumes.take().unwrap_or_default();
                volumes.push(Volume {
                    name: "workspace".to_string(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: pvc_name,
                        read_only: Some(false),
                    }),
                    ..Default::default()
                });
                spec.volumes = Some(volumes);

                // Add volumeMount to the first container
                if let Some(container) = spec.containers.first_mut() {
                    let mut mounts = container.volume_mounts.take().unwrap_or_default();
                    mounts.push(VolumeMount {
                        name: "workspace".to_string(),
                        mount_path: persistence.mount_path.clone(),
                        ..Default::default()
                    });
                    container.volume_mounts = Some(mounts);
                }

                // Set fsGroup so the mounted PVC is writable by the sandbox user
                let sc = spec
                    .security_context
                    .get_or_insert_with(PodSecurityContext::default);
                if sc.fs_group.is_none() {
                    sc.fs_group = Some(1000);
                }
            }
        }

        // Apply per-call resource specs to the first container.
        if let Some(ref mut spec) = pod_template.spec {
            if let Some(container) = spec.containers.first_mut() {
                container.resources = Some(ResourceRequirements {
                    requests: Some(BTreeMap::from([
                        ("cpu".to_string(), Quantity(specs.cpu.clone())),
                        ("memory".to_string(), Quantity(specs.memory.clone())),
                    ])),
                    limits: Some(BTreeMap::from([
                        ("cpu".to_string(), Quantity(specs.cpu.clone())),
                        ("memory".to_string(), Quantity(specs.memory.clone())),
                    ])),
                    ..Default::default()
                });
            }
        }

        let mut labels = BTreeMap::new();
        labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());

        let mut sandbox = Sandbox::new(
            name,
            SandboxSpec {
                pod_template,
                volume_claim_templates: Vec::new(),
                lifecycle: None,
                replicas: 1,
            },
        );
        sandbox.metadata.labels = Some(labels);

        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        sandboxes.create(&PostParams::default(), &sandbox).await?;

        tracing::info!(sandbox = %name, "Sandbox created");
        Ok(SandboxView {
            name: name.to_string(),
            phase: "Provisioning".to_string(),
            ready: false,
            base_url: None,
            service_fqdn: None,
        })
    }

    /// Check if a sandbox's container image matches the current template.
    ///
    /// Returns `true` if the sandbox is up-to-date, `false` if it needs recreation.
    #[instrument(name = "sandbox.admin.k8s.is_sandbox_current", skip_all, fields(%name))]
    pub async fn is_sandbox_current(&self, name: &str) -> Result<bool, AdminError> {
        let template = self.read_template().await?;
        let template_image = template
            .spec
            .pod_template
            .spec
            .as_ref()
            .and_then(|s| s.containers.first())
            .and_then(|c| c.image.as_deref());

        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => AdminError::NotFound(name.to_string()),
            _ => AdminError::Kube(e),
        })?;
        let sandbox_image = sandbox
            .spec
            .pod_template
            .spec
            .as_ref()
            .and_then(|s| s.containers.first())
            .and_then(|c| c.image.as_deref());

        let current = template_image == sandbox_image;
        if !current {
            tracing::info!(
                sandbox = %name,
                template_image = ?template_image,
                sandbox_image = ?sandbox_image,
                "Sandbox image is stale"
            );
        }
        Ok(current)
    }

    /// Delete a Sandbox. The PVC is retained for recreation.
    #[instrument(name = "sandbox.admin.k8s.delete_sandbox", skip_all, fields(%name))]
    pub async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        // Mirror the 404→NotFound mapping in `get_sandbox` /
        // `wait_sandbox_ready` so callers can distinguish "wasn't there"
        // from genuine API failures. Skipping this caused the
        // delete-then-create lifecycle to surface 404 as a generic
        // `AdminError::Kube` and abort every fresh provisioning attempt.
        sandboxes
            .delete(name, &DeleteParams::default())
            .await
            .map_err(|e| match &e {
                kube::Error::Api(resp) if resp.code == 404 => {
                    AdminError::NotFound(name.to_string())
                }
                _ => AdminError::Kube(e),
            })?;
        tracing::info!(sandbox = %name, "Sandbox deleted");
        Ok(())
    }

    /// List sandboxes created by this client (filtered by managed-by label).
    #[instrument(name = "sandbox.admin.k8s.list_sandboxes", skip_all)]
    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, AdminError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels(&format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}"));
        let list = sandboxes.list(&lp).await?;
        Ok(list
            .items
            .iter()
            .map(|s| self.view_from_sandbox(s))
            .collect())
    }

    /// Get a single Sandbox by name.
    #[instrument(name = "sandbox.admin.k8s.get_sandbox", skip_all, fields(%name))]
    pub async fn get_sandbox(&self, name: &str) -> Result<SandboxView, AdminError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => AdminError::NotFound(name.to_string()),
            _ => AdminError::Kube(e),
        })?;
        Ok(self.view_from_sandbox(&sandbox))
    }

    /// Poll a Sandbox until it becomes ready or the timeout expires.
    #[instrument(name = "sandbox.admin.k8s.wait_sandbox_ready", skip_all, fields(%name))]
    pub async fn wait_sandbox_ready(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<SandboxView, AdminError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let view = self.get_sandbox(name).await?;
            if view.ready {
                return Ok(view);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AdminError::Timeout(name.to_string()));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Resolve the running pod for a Sandbox by reading its label selector.
    #[instrument(name = "sandbox.admin.k8s.resolve_sandbox_pod", skip_all, fields(%sandbox_name))]
    pub async fn resolve_sandbox_pod(&self, sandbox_name: &str) -> Result<String, AdminError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(sandbox_name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => {
                AdminError::NotFound(sandbox_name.to_string())
            }
            _ => AdminError::Kube(e),
        })?;

        let label_selector = sandbox
            .status
            .as_ref()
            .and_then(|s| s.label_selector.clone())
            .ok_or_else(|| AdminError::NoPod(sandbox_name.to_string()))?;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_list = pods
            .list(&ListParams::default().labels(&label_selector))
            .await?;

        pod_list
            .items
            .iter()
            .find(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .map(|p| p == "Running")
                    .unwrap_or(false)
            })
            .and_then(|pod| pod.metadata.name.clone())
            .ok_or_else(|| AdminError::NoPod(sandbox_name.to_string()))
    }

    /// Exec into a Sandbox pod with TTY.
    #[instrument(name = "sandbox.admin.k8s.exec_sandbox", skip_all, fields(%sandbox_name))]
    pub async fn exec_sandbox(
        &self,
        sandbox_name: &str,
        command: Vec<String>,
    ) -> Result<AttachedProcess, AdminError> {
        let pod_name = self.resolve_sandbox_pod(sandbox_name).await?;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let attached = pods
            .exec(&pod_name, &command, &AttachParams::interactive_tty())
            .await?;

        tracing::info!(
            sandbox = %sandbox_name,
            pod = %pod_name,
            "Exec session started"
        );
        Ok(attached)
    }

    /// Read the cluster-internal service FQDN for a Sandbox.
    ///
    /// The sandbox controller creates a headless Service for every Sandbox
    /// and records its DNS name in `status.serviceFQDN`.  This is how the
    /// drua server reaches the harness HTTP server inside the pod.
    #[instrument(name = "sandbox.admin.k8s.get_service_fqdn", skip_all, fields(%sandbox_name))]
    pub async fn get_service_fqdn(&self, sandbox_name: &str) -> Result<String, AdminError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(sandbox_name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => {
                AdminError::NotFound(sandbox_name.to_string())
            }
            _ => AdminError::Kube(e),
        })?;

        sandbox
            .status
            .as_ref()
            .and_then(|s| s.service_fqdn.clone())
            .ok_or_else(|| AdminError::NoPod(sandbox_name.to_string()))
    }

    fn view_from_sandbox(&self, sandbox: &Sandbox) -> SandboxView {
        let name = sandbox
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "<unknown>".into());
        let ready = sandbox
            .status
            .as_ref()
            .map(|s| {
                s.conditions.iter().any(|c| {
                    c.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "Ready")
                        .unwrap_or(false)
                        && c.get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s == "True")
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        let phase = if ready {
            "Ready".to_string()
        } else {
            "Provisioning".to_string()
        };

        let service_fqdn = sandbox.status.as_ref().and_then(|s| s.service_fqdn.clone());
        let base_url = service_fqdn
            .as_ref()
            .map(|fqdn| format!("http://{fqdn}:{}", self.sandbox_port));

        SandboxView {
            name,
            phase,
            ready,
            base_url,
            service_fqdn,
        }
    }

    /// Gather diagnostic info about the sandbox infrastructure.
    #[instrument(name = "sandbox.admin.k8s.debug_status", skip_all)]
    pub async fn debug_status(&self) -> Result<serde_json::Value, AdminError> {
        use k8s_openapi::api::core::v1::Event;

        // List all sandboxes
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox_list = sandboxes.list(&ListParams::default()).await?;
        let sandbox_summaries: Vec<serde_json::Value> = sandbox_list
            .items
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.metadata.name,
                    "status": s.status,
                    "replicas": s.spec.replicas,
                })
            })
            .collect();

        // List sandbox templates
        let templates: Api<SandboxTemplate> = Api::namespaced(self.client.clone(), &self.namespace);
        let template_list = templates.list(&ListParams::default()).await?;
        let template_names: Vec<Option<String>> = template_list
            .items
            .iter()
            .map(|t| t.metadata.name.clone())
            .collect();

        // List pods in namespace
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_list = pods.list(&ListParams::default()).await?;
        let pod_summaries: Vec<serde_json::Value> = pod_list
            .items
            .iter()
            .map(|p| {
                let status = p.status.as_ref();
                serde_json::json!({
                    "name": p.metadata.name,
                    "phase": status.and_then(|s| s.phase.clone()),
                    "conditions": status.map(|s| &s.conditions),
                    "container_statuses": status.map(|s| &s.container_statuses),
                })
            })
            .collect();

        // Recent events in namespace
        let events: Api<Event> = Api::namespaced(self.client.clone(), &self.namespace);
        let event_list = events.list(&ListParams::default()).await?;
        let recent_events: Vec<serde_json::Value> = event_list
            .items
            .iter()
            .rev()
            .take(30)
            .map(|e| {
                serde_json::json!({
                    "reason": e.reason,
                    "message": e.message,
                    "type": e.type_,
                    "involved_object": e.involved_object.name,
                    "last_timestamp": e.last_timestamp,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "namespace": self.namespace,
            "template_name": self.template_name,
            "sandboxes": sandbox_summaries,
            "templates": template_names,
            "pods": pod_summaries,
            "recent_events": recent_events,
        }))
    }
}

/// Default workspace root inside the sandbox container, matching the
/// `WORKSPACE_ROOT` default in `images/sandbox/server/src/main.rs`.
const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";

#[async_trait]
impl AdminClient for K8sAdminClient {
    async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<SandboxView, AdminError> {
        K8sAdminClient::create_sandbox(self, name, specs).await
    }

    async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError> {
        K8sAdminClient::delete_sandbox(self, name).await
    }

    async fn get_sandbox(&self, name: &str) -> Result<SandboxView, AdminError> {
        K8sAdminClient::get_sandbox(self, name).await
    }

    async fn list_sandboxes(&self) -> Result<Vec<SandboxView>, AdminError> {
        K8sAdminClient::list_sandboxes(self).await
    }

    async fn wait_sandbox_ready(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<SandboxView, AdminError> {
        K8sAdminClient::wait_sandbox_ready(self, name, timeout).await
    }

    fn mount_path(&self, _name: &str) -> String {
        self.persistence
            .as_ref()
            .map(|p| p.mount_path.clone())
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ROOT.to_string())
    }
}
