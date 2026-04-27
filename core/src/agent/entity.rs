use std::collections::HashSet;

use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::{
    AgentId, AuthScope, AuthSubject, SandboxId, UserId, WorkflowDefinitionId, WorkflowRunId,
    WorkspaceId,
};
use crate::sandbox::SandboxAgentMode;
use es_entity::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum AgentRole {
    WorkspaceLead,
    Agent,
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "AgentId")]
pub enum AgentEvent {
    Initialized {
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: String,
        authz_scopes: Vec<AuthScope>,
        workspace_name: String,
        /// Workflow definition this agent was spawned for, if any.
        /// `None` for user-created agents — those are the ones the
        /// workspace agent listing surfaces by default.
        #[serde(default)]
        workflow_id: Option<WorkflowDefinitionId>,
        /// Specific workflow run that owns this agent. Always `Some`
        /// when `workflow_id` is `Some`. See [`Agent::workflow_run_id`].
        #[serde(default)]
        workflow_run_id: Option<WorkflowRunId>,
    },
    /// Effective delta only (no-ops filtered out before firing).
    AuthScopesUpdated {
        added: Vec<AuthScope>,
        removed: Vec<AuthScope>,
    },
    /// First-time attach or in-place mode upgrade/downgrade.
    SandboxAttached {
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    },
    SandboxDetached {
        sandbox_id: SandboxId,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Agent {
    pub id: AgentId,
    pub workspace_id: WorkspaceId,
    pub agent_role: AgentRole,
    pub name: String,
    /// Set at creation; never updated (rename does not propagate).
    pub workspace_name: String,
    /// `HashSet` enforces no-duplicates; wire format stays `Vec<AuthScope>`
    /// for backward-compatible JSON serialization.
    pub authz_scopes: HashSet<AuthScope>,
    /// At most one attached sandbox at a time (enforced by `sandbox_attached`).
    #[builder(default)]
    pub attached_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    /// Workflow definition this agent was spawned for, if any.
    ///
    /// `None` means the agent was created directly by a user (the normal
    /// case — these are what `Agents::list_for_workspace` returns).
    /// `Some` means the agent was spawned by a [`crate::workflow`] run
    /// to execute one of its steps; those agents are owned by the run
    /// and hidden from the default workspace listing.
    #[builder(default)]
    pub workflow_id: Option<WorkflowDefinitionId>,
    /// Specific workflow run that owns this agent. Populated together
    /// with [`Self::workflow_id`].
    #[builder(default)]
    pub workflow_run_id: Option<WorkflowRunId>,
    events: EntityEvents<AgentEvent>,
}

impl Agent {
    fn scopes_vec(&self) -> Vec<AuthScope> {
        self.authz_scopes.iter().cloned().collect()
    }

    pub fn attached_sandbox_id(&self) -> Option<SandboxId> {
        self.attached_sandbox.map(|(id, _)| id)
    }

    /// Use when no originating user can be attributed.
    pub fn auth_subject(&self) -> AuthSubject {
        AuthSubject::Agent(self.workspace_id, self.id, self.scopes_vec())
    }

    /// Tagged with the originating user so downstream actions can be
    /// attributed back.
    pub fn auth_subject_for_user(&self, user_id: UserId) -> AuthSubject {
        AuthSubject::AgentOnBehalfOfUser(user_id, self.workspace_id, self.id, self.scopes_vec())
    }

    /// Records only the effective delta; returns `AlreadyApplied` when empty.
    fn update_auth_scopes(&mut self, added: &[AuthScope], removed: &[AuthScope]) -> Idempotent<()> {
        let mut effective_added: Vec<AuthScope> = Vec::new();
        for scope in added {
            if self.authz_scopes.insert(scope.clone()) {
                effective_added.push(scope.clone());
            }
        }
        let mut effective_removed: Vec<AuthScope> = Vec::new();
        for scope in removed {
            if self.authz_scopes.remove(scope) {
                effective_removed.push(scope.clone());
            }
        }

        if effective_added.is_empty() && effective_removed.is_empty() {
            return Idempotent::AlreadyApplied;
        }

        self.events.push(AgentEvent::AuthScopesUpdated {
            added: effective_added,
            removed: effective_removed,
        });
        Idempotent::Executed(())
    }

    /// At most one sandbox per agent: a different sandbox returns
    /// `AlreadyAttachedToSandbox`. Same id + same mode is idempotent;
    /// different mode is an in-place upgrade/downgrade. Write implies Read.
    pub(super) fn sandbox_attached(
        &mut self,
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    ) -> Result<Idempotent<()>, super::error::AgentError> {
        if matches!(self.agent_role, AgentRole::WorkspaceLead) {
            return Err(super::error::AgentError::LeadCannotAttachSandbox);
        }

        if let Some((existing_id, existing_mode)) = self.attached_sandbox {
            if existing_id != sandbox_id {
                return Err(super::error::AgentError::AlreadyAttachedToSandbox {
                    current: existing_id,
                });
            }
            if existing_mode == mode {
                return Ok(Idempotent::AlreadyApplied);
            }
        }

        self.attached_sandbox = Some((sandbox_id, mode));
        // Scope delta may be AlreadyApplied; attachment event still fires.
        let _ = self.apply_sandbox_scopes(sandbox_id, mode);
        self.events
            .push(AgentEvent::SandboxAttached { sandbox_id, mode });
        Ok(Idempotent::Executed(()))
    }

    /// Drops both `SandboxUse` and `SandboxRead` for `sandbox_id`.
    /// Idempotent when not attached to that sandbox.
    pub(super) fn sandbox_detached(&mut self, sandbox_id: SandboxId) -> Idempotent<()> {
        let is_attached = matches!(self.attached_sandbox, Some((id, _)) if id == sandbox_id);
        if !is_attached {
            return Idempotent::AlreadyApplied;
        }

        self.attached_sandbox = None;
        let _ = self.update_auth_scopes(
            &[],
            &[
                AuthScope::SandboxUse(sandbox_id),
                AuthScope::SandboxRead(sandbox_id),
            ],
        );
        self.events.push(AgentEvent::SandboxDetached { sandbox_id });
        Idempotent::Executed(())
    }

    /// Read grants `SandboxRead` (removes `SandboxUse`); Write grants
    /// `SandboxUse` (removes `SandboxRead`).
    fn apply_sandbox_scopes(
        &mut self,
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    ) -> Idempotent<()> {
        let (added, removed) = match mode {
            SandboxAgentMode::Read => (
                vec![AuthScope::SandboxRead(sandbox_id)],
                vec![AuthScope::SandboxUse(sandbox_id)],
            ),
            SandboxAgentMode::Write => (
                vec![AuthScope::SandboxUse(sandbox_id)],
                vec![AuthScope::SandboxRead(sandbox_id)],
            ),
        };
        self.update_auth_scopes(&added, &removed)
    }
}

impl TryFromEvents<AgentEvent> for Agent {
    fn try_from_events(events: EntityEvents<AgentEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = AgentBuilder::default();
        let mut scopes: HashSet<AuthScope> = HashSet::new();
        let mut attached_sandbox: Option<(SandboxId, SandboxAgentMode)> = None;

        for event in events.iter_all() {
            match event {
                AgentEvent::Initialized {
                    id,
                    workspace_id,
                    agent_role,
                    name,
                    authz_scopes,
                    workspace_name,
                    workflow_id,
                    workflow_run_id,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_role(*agent_role)
                        .name(name.clone())
                        .workspace_name(workspace_name.clone())
                        .workflow_id(*workflow_id)
                        .workflow_run_id(*workflow_run_id);
                    scopes = authz_scopes.iter().cloned().collect();
                }
                AgentEvent::AuthScopesUpdated { added, removed } => {
                    for s in added {
                        scopes.insert(s.clone());
                    }
                    for s in removed {
                        scopes.remove(s);
                    }
                }
                AgentEvent::SandboxAttached { sandbox_id, mode } => {
                    attached_sandbox = Some((*sandbox_id, *mode));
                }
                AgentEvent::SandboxDetached { sandbox_id } => {
                    if matches!(attached_sandbox, Some((id, _)) if id == *sandbox_id) {
                        attached_sandbox = None;
                    }
                }
            }
        }

        builder
            .authz_scopes(scopes)
            .attached_sandbox(attached_sandbox)
            .events(events)
            .build()
    }
}

#[derive(Debug, Builder)]
pub struct NewAgent {
    #[builder(setter(into))]
    pub(super) id: AgentId,
    pub(super) workspace_id: WorkspaceId,
    pub(super) agent_role: AgentRole,
    #[builder(setter(into))]
    pub(super) name: String,
    pub(super) authz_scopes: Vec<AuthScope>,
    #[builder(setter(into))]
    pub(super) workspace_name: String,
    /// `Some` when the agent is spawned by a workflow run.
    #[builder(default, setter(into, strip_option))]
    pub(super) workflow_id: Option<WorkflowDefinitionId>,
    #[builder(default, setter(into, strip_option))]
    pub(super) workflow_run_id: Option<WorkflowRunId>,
}

impl NewAgent {
    pub fn builder() -> NewAgentBuilder {
        NewAgentBuilder::default()
    }
}

impl IntoEvents<AgentEvent> for NewAgent {
    fn into_events(self) -> EntityEvents<AgentEvent> {
        EntityEvents::init(
            self.id,
            [AgentEvent::Initialized {
                id: self.id,
                workspace_id: self.workspace_id,
                agent_role: self.agent_role,
                name: self.name,
                authz_scopes: self.authz_scopes,
                workspace_name: self.workspace_name,
                workflow_id: self.workflow_id,
                workflow_run_id: self.workflow_run_id,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn build(workflow_id: Option<WorkflowDefinitionId>, run_id: Option<WorkflowRunId>) -> Agent {
        let mut builder = NewAgent::builder();
        builder
            .id(AgentId::new())
            .workspace_id(WorkspaceId::new())
            .agent_role(AgentRole::Agent)
            .name("test")
            .authz_scopes(Vec::new())
            .workspace_name("ws");
        if let Some(wf) = workflow_id {
            builder.workflow_id(wf);
        }
        if let Some(r) = run_id {
            builder.workflow_run_id(r);
        }
        let new = builder.build().unwrap();
        Agent::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn user_owned_agent_has_no_workflow_refs() {
        let agent = build(None, None);
        assert!(agent.workflow_id.is_none());
        assert!(agent.workflow_run_id.is_none());
    }

    #[test]
    fn workflow_agent_carries_both_refs() {
        let wf = WorkflowDefinitionId::new();
        let run = WorkflowRunId::new();
        let agent = build(Some(wf), Some(run));
        assert_eq!(agent.workflow_id, Some(wf));
        assert_eq!(agent.workflow_run_id, Some(run));
    }
}
