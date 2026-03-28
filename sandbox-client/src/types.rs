use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Sandbox  (agents.x-k8s.io/v1alpha1)
// ---------------------------------------------------------------------------

/// Sandbox manages a single, stateful, isolated pod with a stable identity.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.x-k8s.io",
    version = "v1alpha1",
    kind = "Sandbox",
    plural = "sandboxes",
    status = "SandboxStatus",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    /// Pod template describing the sandbox container(s).
    pub pod_template: PodTemplateSpec,

    /// Optional persistent volume claim templates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_claim_templates: Vec<serde_json::Value>,

    /// Lifecycle controls (expiration, shutdown policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<SandboxLifecycle>,

    /// Only 0 (paused) or 1 (active) are valid.
    #[serde(default = "default_replicas")]
    pub replicas: i32,
}

fn default_replicas() -> i32 {
    1
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxLifecycle {
    /// Absolute time when the sandbox should be shut down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_time: Option<Time>,

    /// What to do when the sandbox is shut down: "Delete" or "Retain".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_policy: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    /// Standard Kubernetes conditions (e.g. Ready).
    #[serde(default)]
    pub conditions: Vec<serde_json::Value>,

    /// Name of the headless service created for this sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// Cluster-internal DNS name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_fqdn: Option<String>,

    /// Current replica count.
    #[serde(default)]
    pub replicas: i32,

    /// Label selector for pod discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<String>,
}

// ---------------------------------------------------------------------------
// SandboxTemplate  (extensions.agents.x-k8s.io/v1alpha1)
// ---------------------------------------------------------------------------

/// Reusable template for creating Sandbox instances.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "extensions.agents.x-k8s.io",
    version = "v1alpha1",
    kind = "SandboxTemplate",
    plural = "sandboxtemplates",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxTemplateSpec {
    /// Pod template to use for sandboxes created from this template.
    pub pod_template: PodTemplateSpec,

    /// NetworkPolicy management mode: "Managed" (default) or "Unmanaged".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy_management: Option<String>,

    /// Custom network policy rules (when Unmanaged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// SandboxClaim  (extensions.agents.x-k8s.io/v1alpha1)
// ---------------------------------------------------------------------------

/// Claims a sandbox from a template (or warm pool).
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "extensions.agents.x-k8s.io",
    version = "v1alpha1",
    kind = "SandboxClaim",
    plural = "sandboxclaims",
    status = "SandboxClaimStatus",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxClaimSpec {
    /// Reference to a SandboxTemplate.
    pub sandbox_template_ref: TemplateRef,

    /// Lifecycle configuration for the claimed sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<SandboxLifecycle>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TemplateRef {
    /// Name of the SandboxTemplate.
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxClaimStatus {
    /// Mirrors the underlying Sandbox status conditions.
    #[serde(default)]
    pub conditions: Vec<serde_json::Value>,

    /// Reference to the Sandbox created by this claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxClaimSandboxRef>,
}

/// Nested object in SandboxClaim status that references the bound Sandbox.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct SandboxClaimSandboxRef {
    /// Name of the Sandbox (CRD uses uppercase "Name").
    #[serde(rename = "Name")]
    pub name: String,
}

// ---------------------------------------------------------------------------
// SandboxWarmPool  (extensions.agents.x-k8s.io/v1alpha1)
// ---------------------------------------------------------------------------

/// Maintains a pool of pre-warmed sandbox pods for fast allocation.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "extensions.agents.x-k8s.io",
    version = "v1alpha1",
    kind = "SandboxWarmPool",
    plural = "sandboxwarmpools",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxWarmPoolSpec {
    /// Reference to a SandboxTemplate from which to pre-create sandboxes.
    pub sandbox_template_ref: TemplateRef,

    /// Number of pre-warmed sandbox replicas to maintain.
    pub replicas: i32,
}
