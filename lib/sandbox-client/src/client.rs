use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, Pod,
    PodSecurityContext, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, AttachParams, AttachedProcess, DeleteParams, ListParams, PostParams};
use kube::Client;
use tracing::instrument;

use crate::error::SandboxError;
use crate::types::*;

const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
const MANAGED_BY_VALUE: &str = "galoy-agents";

/// Configuration for persistent storage on sandbox pods.
#[derive(Clone, Debug)]
pub struct PersistenceConfig {
    pub size: String,
    pub storage_class: String,
    pub mount_path: String,
}

/// High-level client for managing Agent Sandbox resources.
#[derive(Clone)]
pub struct SandboxClient {
    client: Client,
    namespace: String,
    template_name: String,
    persistence: Option<PersistenceConfig>,
}

/// Summary of a sandbox for API consumers.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SandboxSummary {
    pub name: String,
    pub sandbox_name: Option<String>,
    pub phase: String,
    pub ready: bool,
}

impl SandboxClient {
    /// Create a new sandbox client for the given namespace and template.
    pub fn new(client: Client, namespace: String, template_name: String) -> Self {
        Self {
            client,
            namespace,
            template_name,
            persistence: None,
        }
    }

    /// Enable persistent storage for directly-created sandboxes.
    pub fn with_persistence(mut self, config: PersistenceConfig) -> Self {
        self.persistence = Some(config);
        self
    }

    /// Whether persistent storage is configured for sandboxes.
    pub fn has_persistence(&self) -> bool {
        self.persistence.is_some()
    }

    /// The namespace this client manages sandboxes in.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Build a client from in-cluster or kubeconfig environment.
    pub async fn try_from_env(
        namespace: String,
        template_name: String,
    ) -> Result<Self, SandboxError> {
        let client = Client::try_default().await?;
        Ok(Self::new(client, namespace, template_name))
    }

    /// Create a SandboxClaim that allocates a sandbox from the warm pool.
    #[instrument(name = "sandbox_client.create_claim", skip_all, fields(%claim_name))]
    pub async fn create_claim(&self, claim_name: &str) -> Result<SandboxClaim, SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);

        let claim = SandboxClaim::new(
            claim_name,
            SandboxClaimSpec {
                sandbox_template_ref: TemplateRef {
                    name: self.template_name.clone(),
                },
                lifecycle: None,
            },
        );

        let created = claims.create(&PostParams::default(), &claim).await?;
        tracing::info!(claim = %claim_name, "Sandbox claim created");
        Ok(created)
    }

    /// Delete a SandboxClaim, releasing the sandbox back.
    #[instrument(name = "sandbox_client.delete_claim", skip_all, fields(%claim_name))]
    pub async fn delete_claim(&self, claim_name: &str) -> Result<(), SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        claims.delete(claim_name, &DeleteParams::default()).await?;
        tracing::info!(claim = %claim_name, "Sandbox claim deleted");
        Ok(())
    }

    /// List all SandboxClaims in the namespace.
    #[instrument(name = "sandbox_client.list_claims", skip_all)]
    pub async fn list_claims(&self) -> Result<Vec<SandboxSummary>, SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = claims.list(&ListParams::default()).await?;

        let summaries = list.items.iter().map(Self::summary_from_claim).collect();

        Ok(summaries)
    }

    /// Get a single SandboxClaim by name.
    #[instrument(name = "sandbox_client.get_claim", skip_all, fields(%claim_name))]
    pub async fn get_claim(&self, claim_name: &str) -> Result<SandboxSummary, SandboxError> {
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let claim = claims.get(claim_name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => {
                SandboxError::NotFound(claim_name.to_string())
            }
            _ => SandboxError::Kube(e),
        })?;

        Ok(Self::summary_from_claim(&claim))
    }

    /// Poll a SandboxClaim until it becomes ready or the timeout expires.
    #[instrument(name = "sandbox_client.wait_until_ready", skip_all, fields(%claim_name))]
    pub async fn wait_until_ready(
        &self,
        claim_name: &str,
        timeout: Duration,
    ) -> Result<SandboxSummary, SandboxError> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let summary = self.get_claim(claim_name).await?;
            if summary.ready {
                return Ok(summary);
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(SandboxError::Timeout(claim_name.to_string()));
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Resolve the running pod name for a sandbox claim.
    ///
    /// Follows the chain: claim → sandbox_name → Sandbox CR label_selector → Pod list.
    /// Resolve the pod name for a claim-based sandbox.
    #[instrument(name = "sandbox_client.resolve_pod_name", skip_all, fields(%claim_name))]
    pub async fn resolve_pod_name(&self, claim_name: &str) -> Result<String, SandboxError> {
        let summary = self.get_claim(claim_name).await?;
        if !summary.ready {
            return Err(SandboxError::NotReady(claim_name.to_string()));
        }

        let sandbox_name = summary
            .sandbox_name
            .ok_or_else(|| SandboxError::NoPod(claim_name.to_string()))?;

        self.resolve_pod_for_sandbox(&sandbox_name).await
    }

    /// Resolve the pod name for a directly-created sandbox.
    #[instrument(name = "sandbox_client.resolve_pod_for_sandbox", skip_all, fields(%sandbox_name))]
    pub async fn resolve_pod_for_sandbox(
        &self,
        sandbox_name: &str,
    ) -> Result<String, SandboxError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(sandbox_name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => {
                SandboxError::NotFound(sandbox_name.to_string())
            }
            _ => SandboxError::Kube(e),
        })?;

        let label_selector = sandbox
            .status
            .as_ref()
            .and_then(|s| s.label_selector.clone())
            .ok_or_else(|| SandboxError::NoPod(sandbox_name.to_string()))?;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_list = pods
            .list(&ListParams::default().labels(&label_selector))
            .await?;

        let pod_name = pod_list
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
            .ok_or_else(|| SandboxError::NoPod(sandbox_name.to_string()))?;

        Ok(pod_name)
    }

    /// Exec into the sandbox pod associated with a claim.
    #[instrument(name = "sandbox_client.exec_in_sandbox", skip_all, fields(%claim_name))]
    pub async fn exec_in_sandbox(
        &self,
        claim_name: &str,
        command: Vec<String>,
    ) -> Result<AttachedProcess, SandboxError> {
        let pod_name = if self.has_persistence() {
            self.resolve_pod_for_sandbox(claim_name).await?
        } else {
            self.resolve_pod_name(claim_name).await?
        };

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let attached = pods
            .exec(&pod_name, &command, &AttachParams::interactive_tty())
            .await?;

        tracing::info!(
            claim = %claim_name,
            pod = %pod_name,
            "Exec session started"
        );

        Ok(attached)
    }

    // -----------------------------------------------------------------------
    // Direct Sandbox management (bypasses SandboxClaim / warm pool)
    // -----------------------------------------------------------------------

    /// Read the SandboxTemplate to get the pod template.
    #[instrument(name = "sandbox_client.read_template", skip_all)]
    async fn read_template(&self) -> Result<SandboxTemplate, SandboxError> {
        let templates: Api<SandboxTemplate> = Api::namespaced(self.client.clone(), &self.namespace);
        templates
            .get(&self.template_name)
            .await
            .map_err(SandboxError::Kube)
    }

    /// Ensure a standalone PVC exists for the given sandbox name.
    /// Creates the PVC if it doesn't exist; no-ops if it already does.
    #[instrument(name = "sandbox_client.ensure_pvc", skip_all, fields(%name))]
    async fn ensure_pvc(
        &self,
        name: &str,
        persistence: &PersistenceConfig,
    ) -> Result<String, SandboxError> {
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
                                Quantity(persistence.size.clone()),
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
            Err(e) => return Err(SandboxError::Kube(e)),
        }

        Ok(pvc_name)
    }

    /// Create a Sandbox directly with optional persistent storage.
    ///
    /// When persistence is configured, creates a standalone PVC (if it doesn't
    /// already exist) and mounts it into the pod. The PVC lifecycle is
    /// independent of the Sandbox — it survives sandbox deletion and is
    /// reused on recreation with the same name.
    #[instrument(name = "sandbox_client.create_sandbox", skip_all, fields(%name))]
    pub async fn create_sandbox(&self, name: &str) -> Result<SandboxSummary, SandboxError> {
        let template = self.read_template().await?;
        let mut pod_template = template.spec.pod_template;

        if let Some(ref persistence) = self.persistence {
            let pvc_name = self.ensure_pvc(name, persistence).await?;

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

        tracing::info!(sandbox = %name, "Sandbox created directly");
        Ok(SandboxSummary {
            name: name.to_string(),
            sandbox_name: Some(name.to_string()),
            phase: "Provisioning".to_string(),
            ready: false,
        })
    }

    /// Delete a directly-created Sandbox. The PVC is retained for recreation.
    #[instrument(name = "sandbox_client.delete_sandbox", skip_all, fields(%name))]
    pub async fn delete_sandbox(&self, name: &str) -> Result<(), SandboxError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        sandboxes.delete(name, &DeleteParams::default()).await?;
        tracing::info!(sandbox = %name, "Sandbox deleted");
        Ok(())
    }

    /// List sandboxes created by this client (filtered by managed-by label).
    #[instrument(name = "sandbox_client.list_sandboxes", skip_all)]
    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxSummary>, SandboxError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels(&format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}"));
        let list = sandboxes.list(&lp).await?;
        Ok(list.items.iter().map(Self::summary_from_sandbox).collect())
    }

    /// Get a single Sandbox by name.
    #[instrument(name = "sandbox_client.get_sandbox", skip_all, fields(%name))]
    pub async fn get_sandbox(&self, name: &str) -> Result<SandboxSummary, SandboxError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => SandboxError::NotFound(name.to_string()),
            _ => SandboxError::Kube(e),
        })?;
        Ok(Self::summary_from_sandbox(&sandbox))
    }

    /// Poll a Sandbox until it becomes ready or the timeout expires.
    #[instrument(name = "sandbox_client.wait_sandbox_ready", skip_all, fields(%name))]
    pub async fn wait_sandbox_ready(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<SandboxSummary, SandboxError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let summary = self.get_sandbox(name).await?;
            if summary.ready {
                return Ok(summary);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SandboxError::Timeout(name.to_string()));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Resolve the running pod for a Sandbox by reading its label selector.
    #[instrument(name = "sandbox_client.resolve_sandbox_pod", skip_all, fields(%sandbox_name))]
    pub async fn resolve_sandbox_pod(&self, sandbox_name: &str) -> Result<String, SandboxError> {
        let sandboxes: Api<Sandbox> = Api::namespaced(self.client.clone(), &self.namespace);
        let sandbox = sandboxes.get(sandbox_name).await.map_err(|e| match &e {
            kube::Error::Api(resp) if resp.code == 404 => {
                SandboxError::NotFound(sandbox_name.to_string())
            }
            _ => SandboxError::Kube(e),
        })?;

        let label_selector = sandbox
            .status
            .as_ref()
            .and_then(|s| s.label_selector.clone())
            .ok_or_else(|| SandboxError::NoPod(sandbox_name.to_string()))?;

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
            .ok_or_else(|| SandboxError::NoPod(sandbox_name.to_string()))
    }

    /// Exec into a directly-created Sandbox pod.
    #[instrument(name = "sandbox_client.exec_sandbox", skip_all, fields(%sandbox_name))]
    pub async fn exec_sandbox(
        &self,
        sandbox_name: &str,
        command: Vec<String>,
    ) -> Result<AttachedProcess, SandboxError> {
        let pod_name = self.resolve_sandbox_pod(sandbox_name).await?;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let attached = pods
            .exec(&pod_name, &command, &AttachParams::interactive_tty())
            .await?;

        tracing::info!(
            sandbox = %sandbox_name,
            pod = %pod_name,
            "Exec session started (direct)"
        );
        Ok(attached)
    }

    /// Exec into a Sandbox pod without TTY allocation.
    ///
    /// Unlike [`exec_sandbox`](Self::exec_sandbox), this uses raw stdin/stdout
    /// without a pseudo-terminal.  Suitable for programmatic communication
    /// (e.g. JSON-lines) where TTY escape codes would corrupt the data stream.
    #[instrument(name = "sandbox_client.exec_sandbox_raw", skip_all, fields(%sandbox_name))]
    pub async fn exec_sandbox_raw(
        &self,
        sandbox_name: &str,
        command: Vec<String>,
    ) -> Result<AttachedProcess, SandboxError> {
        let pod_name = self.resolve_sandbox_pod(sandbox_name).await?;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let ap = AttachParams {
            stdin: true,
            stdout: true,
            stderr: true,
            tty: false,
            ..Default::default()
        };
        let attached = pods.exec(&pod_name, &command, &ap).await?;

        tracing::info!(
            sandbox = %sandbox_name,
            pod = %pod_name,
            "Raw exec session started (direct)"
        );
        Ok(attached)
    }

    fn summary_from_sandbox(sandbox: &Sandbox) -> SandboxSummary {
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

        SandboxSummary {
            name: name.clone(),
            sandbox_name: Some(name),
            phase,
            ready,
        }
    }

    /// Gather diagnostic info about the sandbox infrastructure.
    #[instrument(name = "sandbox_client.debug_status", skip_all)]
    pub async fn debug_status(&self) -> Result<serde_json::Value, SandboxError> {
        use k8s_openapi::api::core::v1::Event;

        // List all sandbox claims
        let claims: Api<SandboxClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let claim_list = claims.list(&ListParams::default()).await?;
        let claim_summaries: Vec<serde_json::Value> = claim_list
            .items
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.metadata.name,
                    "status": c.status,
                    "spec": c.spec,
                })
            })
            .collect();

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

        // List warm pools
        let pools: Api<SandboxWarmPool> = Api::namespaced(self.client.clone(), &self.namespace);
        let pool_list = pools.list(&ListParams::default()).await?;
        let pool_summaries: Vec<serde_json::Value> = pool_list
            .items
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.metadata.name,
                    "spec": p.spec,
                    "status": p.status,
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
            "claims": claim_summaries,
            "sandboxes": sandbox_summaries,
            "warm_pools": pool_summaries,
            "templates": template_names,
            "pods": pod_summaries,
            "recent_events": recent_events,
        }))
    }

    fn summary_from_claim(claim: &SandboxClaim) -> SandboxSummary {
        let name = claim
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "<unknown>".into());
        let status = claim.status.as_ref();
        let sandbox_name = status.and_then(|s| s.sandbox.as_ref().map(|r| r.name.clone()));
        let ready = status
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
        } else if sandbox_name.is_some() {
            "Provisioning".to_string()
        } else {
            "Pending".to_string()
        };

        SandboxSummary {
            name,
            sandbox_name,
            phase,
            ready,
        }
    }
}
