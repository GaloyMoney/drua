use serde::{Deserialize, Serialize};

/// Workspace bootstrap mode for the first `/initialize` call. Mirrors the
/// wire format expected by `images/sandbox/server/src/main.rs::initialize`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SandboxMode {
    Scratch,
    /// Clones `repo_url` into `<workspace>/repos/<name>` and scans for
    /// CLAUDE.md / `.claude/commands/*.md`.
    Repo {
        repo_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// Sparse-checkout of the library knowledge-base repo limited to
    /// `spaces/<slug>/`. `slug` will become a typed `SpaceId` once the
    /// `Space` entity lands; for now it's the raw directory name.
    LibrarySpace {
        slug: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_space_round_trips_through_serde() {
        let mode = SandboxMode::LibrarySpace {
            slug: "oncall".to_string(),
        };
        let json = serde_json::to_value(&mode).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "mode": "library_space", "slug": "oncall" })
        );
        let back: SandboxMode = serde_json::from_value(json).unwrap();
        match back {
            SandboxMode::LibrarySpace { slug } => assert_eq!(slug, "oncall"),
            other => panic!("expected LibrarySpace, got {other:?}"),
        }
    }
}

/// K8s applies these to pod resources and the PVC; the local backend logs
/// them but doesn't enforce them.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SandboxSpecs {
    /// e.g. `"500m"` or `"2"`.
    pub cpu: String,
    /// e.g. `"512Mi"` or `"2Gi"`.
    pub memory: String,
    /// e.g. `"10Gi"`.
    pub disk_size: String,
}

/// Mirrors the Kubernetes view; the local backend fills `None` for
/// cluster-only fields.
#[derive(Clone, Debug, Serialize)]
pub struct Sandbox {
    pub name: String,

    /// K8s: `"Provisioning"` then `"Ready"`. Local: `"Ready"` once the
    /// spawned process is accepting connections.
    pub phase: String,

    pub ready: bool,

    /// e.g. `http://127.0.0.1:34567`. `None` until ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// K8s-only headless service FQDN.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_fqdn: Option<String>,
}
