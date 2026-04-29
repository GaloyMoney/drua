//! Pure-data shapes for a workflow definition. Persisted verbatim
//! inside `WorkflowDefinitionEvent` and snapshotted on every
//! `WorkflowRunEvent::Initialized` — wire-format changes here must
//! stay backward-compatible.

use serde::{Deserialize, Serialize};

use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTrigger {
    Manual,
    Webhook {
        /// `Some("honeycomb")` selects the `X-Honeycomb-Webhook-Token`
        /// header; `None` falls back to `Authorization: Bearer`.
        provider: Option<String>,
        secret: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStepDef {
    AgentStep {
        name: String,
        skill: String,
        sandbox: Option<String>,
        /// Read or Write attach mode for the named sandbox.
        /// `None` → `Write`, preserving the original default.
        #[serde(default)]
        sandbox_mode: Option<SandboxAgentMode>,
        timeout_seconds: Option<u64>,
    },
}

impl WorkflowStepDef {
    pub fn name(&self) -> &str {
        match self {
            WorkflowStepDef::AgentStep { name, .. } => name,
        }
    }
}

/// Top-level sandbox declaration on a workflow.
///
/// `Provisioned` decls are workflow-scoped: the executor brings them
/// up before the first step (fresh per run, modulo the workflow-scope
/// reuse handled by the sandbox repo) and suspends them after.
///
/// `Preexisting` decls reference an existing sandbox in the workflow's
/// workspace by its name (workspace-unique via the `(workspace_id,
/// name)` constraint on `sandboxes`). The executor only attaches and
/// detaches; it never creates, restarts, or suspends. The user owns
/// the sandbox lifecycle.
///
/// `type` is the discriminator on the wire (matching `WorkflowStepDef`
/// and `WorkflowTrigger` conventions in this module).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowSandboxDecl {
    Provisioned {
        name: String,
        #[serde(flatten)]
        mode: SandboxMode,
        #[serde(default)]
        specs: Option<SandboxSpecs>,
    },
    Preexisting {
        name: String,
    },
}

impl WorkflowSandboxDecl {
    pub fn name(&self) -> &str {
        match self {
            Self::Provisioned { name, .. } | Self::Preexisting { name } => name,
        }
    }

    /// `None` for `Preexisting` (the user-managed sandbox already
    /// has its own specs); defaults match the MCP `sandbox create`
    /// tool for `Provisioned` decls without an explicit override.
    pub fn specs_or_default(&self) -> Option<SandboxSpecs> {
        match self {
            Self::Provisioned { specs, .. } => {
                Some(specs.clone().unwrap_or_else(|| SandboxSpecs {
                    cpu: "500m".to_string(),
                    memory: "512Mi".to_string(),
                    disk_size: "10Gi".to_string(),
                }))
            }
            Self::Preexisting { .. } => None,
        }
    }
}
