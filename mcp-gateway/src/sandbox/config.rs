use serde::Deserialize;

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Enable Agent Sandbox integration.
    #[serde(default)]
    pub enabled: bool,

    /// Kubernetes namespace where sandboxes are managed.
    #[serde(default)]
    pub namespace: String,

    /// Name of the SandboxTemplate to use when creating claims.
    #[serde(default)]
    pub template_name: String,
}
