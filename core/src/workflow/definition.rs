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

/// Top-level sandbox declaration on a workflow. The executor brings these
/// up before the first step and suspends them after the run finishes;
/// the entity is shared across runs of the same workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSandboxDecl {
    pub name: String,
    #[serde(flatten)]
    pub mode: SandboxMode,
    #[serde(default)]
    pub specs: Option<SandboxSpecs>,
}

impl WorkflowSandboxDecl {
    /// Defaults match the MCP `sandbox create` tool defaults.
    pub fn specs_or_default(&self) -> SandboxSpecs {
        self.specs.clone().unwrap_or_else(|| SandboxSpecs {
            cpu: "500m".to_string(),
            memory: "512Mi".to_string(),
            disk_size: "10Gi".to_string(),
        })
    }
}
