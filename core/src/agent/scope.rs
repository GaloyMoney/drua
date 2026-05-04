//! Per-agent scope materialisation. Resolved once per `send_message` (on
//! cache miss) and fed to the skills/notes context builders so the rendered
//! system blocks reflect everything the agent actually has access to:
//! the project it lives in, any sandbox attached to it, and any spaces the
//! project has mounted.
//!
//! Adding a new tier (e.g. workflow-definition skills) is one new field +
//! one extra query in the resolution path; no cache-key reshuffling.

use crate::primitives::{ProjectId, SandboxId, SpaceId};
use crate::project::Projects;

use super::Agent;

#[derive(Debug, Clone)]
pub struct AgentScope {
    pub project_id: ProjectId,
    pub attached_sandbox_id: Option<SandboxId>,
    /// Spaces the agent's project has mounted, in mount order.
    /// Bounded by `Project.mounted_spaces` (currently capped at 20 by
    /// `SPACES_BLOCK_LIMIT` in the `<spaces>` renderer).
    pub mounted_space_ids: Vec<SpaceId>,
}

impl AgentScope {
    /// Materialise the scope for `agent`. Reads `project.mounted_spaces`
    /// (one indexed query). Doesn't auth-check the caller — `for_agent` is
    /// invoked from inside `Agents::cached_dynamic_blocks` after the agent
    /// has already passed the entry-point auth check.
    pub async fn for_agent(
        agent: &Agent,
        projects: &Projects,
    ) -> Result<Self, crate::project::ProjectError> {
        let mounted_space_ids = projects
            .mounted_space_ids_for_project(agent.project_id)
            .await?;
        Ok(Self {
            project_id: agent.project_id,
            attached_sandbox_id: agent.attached_sandbox_id(),
            mounted_space_ids,
        })
    }
}
