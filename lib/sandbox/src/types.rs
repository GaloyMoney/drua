use serde::{Deserialize, Serialize};

/// How the sandbox should bootstrap its workspace on the first
/// `/initialize` call. Mirrors the wire-format `mode` discriminator
/// expected by `images/sandbox/server/src/main.rs::initialize`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SandboxMode {
    /// Empty workspace — `/initialize` just ensures the dir exists.
    Scratch,
    /// `/initialize` clones `repo_url` into `<workspace>/repos/<name>`
    /// and scans the result for CLAUDE.md / .claude/commands/*.md.
    Repo { repo_url: String },
}

/// Per-sandbox resource specs supplied at create time.
///
/// The k8s backend applies these to the pod's resource requests/limits and
/// the PVC's requested storage. The local backend logs them for visibility
/// but doesn't enforce them.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SandboxSpecs {
    /// Kubernetes-style CPU quantity, e.g. `"500m"` or `"2"`.
    pub cpu: String,
    /// Kubernetes-style memory quantity, e.g. `"512Mi"` or `"2Gi"`.
    pub memory: String,
    /// Kubernetes-style storage quantity for the workspace PVC, e.g. `"10Gi"`.
    pub disk_size: String,
}

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
