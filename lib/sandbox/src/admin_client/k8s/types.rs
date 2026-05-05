use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Sandbox CRD (agents.x-k8s.io/v1alpha1): a single, stateful, isolated pod
/// with a stable identity.
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
    pub pod_template: PodTemplateSpec,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_claim_templates: Vec<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<SandboxLifecycle>,

    /// "Delete" or "Retain". The CRD default is "Retain"; sandboxes with
    /// controller-owned ephemeral volumes should opt into "Delete".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_policy: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_time: Option<Time>,

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_time: Option<Time>,

    /// "Delete" or "Retain".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_policy: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    #[serde(default)]
    pub conditions: Vec<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "serviceFQDN"
    )]
    pub service_fqdn: Option<String>,

    #[serde(default)]
    pub replicas: i32,

    #[serde(default, skip_serializing_if = "Option::is_none", rename = "selector")]
    pub label_selector: Option<String>,
}

/// SandboxTemplate CRD (extensions.agents.x-k8s.io/v1alpha1).
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
    pub pod_template: PodTemplateSpec,

    /// "Managed" (default) or "Unmanaged".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy_management: Option<String>,

    /// Used when `network_policy_management == "Unmanaged"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<serde_json::Value>,
}
