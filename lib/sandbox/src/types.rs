use serde::Serialize;

/// Unified handle to a sandbox.
///
/// The shape mirrors the Kubernetes view; the local backend fills `None` /
/// defaults for fields that only make sense in cluster mode.
#[derive(Clone, Debug, Serialize)]
pub struct Sandbox {
    /// Stable identifier (k8s `metadata.name` or local sandbox id).
    pub name: String,

    /// Lifecycle phase. K8s reports `"Provisioning"` until ready then
    /// `"Ready"`; the local backend reports `"Ready"` once the spawned
    /// process is accepting connections.
    pub phase: String,

    /// True when the sandbox is ready to accept HTTP requests.
    pub ready: bool,

    /// Base URL (e.g. `http://127.0.0.1:34567`) for HTTP requests to
    /// `/initialize`, `/execute`, etc. `None` until the sandbox is ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// K8s-only: cluster-internal headless service FQDN created by the
    /// sandbox controller. Always `None` for the local backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_fqdn: Option<String>,
}
