use std::collections::HashSet;

use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use crate::primitives::{AgentId, AuthScope, AuthSubject, SandboxId, UserId, WorkspaceId};
use crate::sandbox::SandboxAgentMode;
use es_entity::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum AgentRole {
    WorkspaceLead,
    /// Generic agent role created on demand (e.g. from the workspace UI).
    /// Defaults to the same authz scopes as [`Self::WorkspaceLead`] and
    /// reuses its `RoleConfig` when a dedicated `agent:` block is absent.
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
    },
    /// Single delta event for adds and removes — bundles both sides of an
    /// attach/detach into one record. `added` and `removed` carry only the
    /// effective delta (no-ops are filtered out before the event fires).
    AuthScopesUpdated {
        added: Vec<AuthScope>,
        removed: Vec<AuthScope>,
    },
    /// The agent has been attached to `sandbox_id` in `mode`. Records both
    /// first-time attachment and mode upgrade/downgrade on the same sandbox.
    SandboxAttached {
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    },
    /// The agent has been detached from `sandbox_id`.
    SandboxDetached { sandbox_id: SandboxId },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Agent {
    pub id: AgentId,
    pub workspace_id: WorkspaceId,
    pub agent_role: AgentRole,
    pub name: String,
    /// Backed by a `HashSet` to enforce no-duplicates at the entity level.
    /// Wire format (`Initialized.authz_scopes`, `AuthScopesUpdated.added/removed`)
    /// stays as `Vec<AuthScope>` for backward-compatible JSON serialization.
    pub authz_scopes: HashSet<AuthScope>,
    /// Which sandbox this agent is currently attached to, if any. The entity
    /// enforces **at most one** attached sandbox at a time via
    /// [`Self::sandbox_attached`] — to switch sandboxes the caller must first
    /// [`Self::sandbox_detached`] the existing one.
    #[builder(default)]
    pub attached_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    events: EntityEvents<AgentEvent>,
}

impl Agent {
    fn scopes_vec(&self) -> Vec<AuthScope> {
        self.authz_scopes.iter().cloned().collect()
    }

    /// The auth subject this agent acts as when invoking tools — its own
    /// workspace + id, carrying the scopes persisted on the `Initialized`
    /// event. Use when no originating user can be attributed.
    pub fn auth_subject(&self) -> AuthSubject {
        AuthSubject::Agent(self.workspace_id, self.id, self.scopes_vec())
    }

    /// Same as [`Self::auth_subject`] but tagged with the user that triggered
    /// the agent's work, so downstream actions can be attributed back to them.
    pub fn auth_subject_for_user(&self, user_id: UserId) -> AuthSubject {
        AuthSubject::AgentOnBehalfOfUser(user_id, self.workspace_id, self.id, self.scopes_vec())
    }

    /// Apply a batch of scope additions/removals as a single
    /// [`AgentEvent::AuthScopesUpdated`] event. Only entries that actually
    /// change the set are recorded — passing scopes the agent already has
    /// (or doesn't have) is a no-op for that entry. Returns
    /// [`Idempotent::AlreadyApplied`] when the resulting delta is empty.
    fn update_auth_scopes(&mut self, added: &[AuthScope], removed: &[AuthScope]) -> Idempotent<()> {
        // `HashSet::insert` returns true iff the value wasn't already present;
        // `HashSet::remove` returns true iff it was. Use those directly so the
        // mutation and the "did it change?" check happen in one pass.
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

    /// Record that this agent is now attached to `sandbox_id` in `mode`.
    ///
    /// **Invariant:** an agent can hold at most one sandbox attachment at a
    /// time. Attaching to a sandbox other than the one already attached
    /// returns [`super::error::AgentError::AlreadyAttachedToSandbox`] —
    /// callers must [`Self::sandbox_detached`] the existing sandbox first.
    ///
    /// Re-attaching the same sandbox with the same mode is idempotent; the
    /// same sandbox with a different mode is an in-place upgrade/downgrade
    /// (the mode switches and scopes are updated). **Write always implies
    /// Read**, so attaching as Write grants both; downgrading to Read
    /// removes the Write scope.
    pub(super) fn sandbox_attached(
        &mut self,
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    ) -> Result<Idempotent<()>, super::error::AgentError> {
        // Leads orchestrate other agents — they never run inside a sandbox
        // themselves.
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
        // Scope delta may be AlreadyApplied (e.g. on an upgrade that only
        // adds an already-held scope); we still record the attachment
        // event below.
        let _ = self.apply_sandbox_scopes(sandbox_id, mode);
        self.events
            .push(AgentEvent::SandboxAttached { sandbox_id, mode });
        Ok(Idempotent::Executed(()))
    }

    /// Record that this agent has been detached from `sandbox_id` — clears
    /// the tracked attachment and drops both `SandboxUseAll` and
    /// `SandboxUseReadOnly` scopes for that id. Idempotent: no-op when the
    /// agent isn't attached to that sandbox.
    pub(super) fn sandbox_detached(&mut self, sandbox_id: SandboxId) -> Idempotent<()> {
        let is_attached = matches!(self.attached_sandbox, Some((id, _)) if id == sandbox_id);
        if !is_attached {
            return Idempotent::AlreadyApplied;
        }

        self.attached_sandbox = None;
        // Scope removal may be AlreadyApplied if the agent somehow didn't
        // have the scopes (defensive); the detach event still fires.
        let _ = self.update_auth_scopes(
            &[],
            &[
                AuthScope::SandboxUseAll(sandbox_id),
                AuthScope::SandboxUseReadOnly(sandbox_id),
            ],
        );
        self.events.push(AgentEvent::SandboxDetached { sandbox_id });
        Idempotent::Executed(())
    }

    /// Apply the correct scope delta for an attachment `mode`. Read mode
    /// grants `SandboxUseReadOnly` (and removes `SandboxUseAll` in case of
    /// downgrade); Write mode grants `SandboxUseAll` (and removes the
    /// read-only scope).
    fn apply_sandbox_scopes(
        &mut self,
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    ) -> Idempotent<()> {
        let (added, removed) = match mode {
            SandboxAgentMode::Read => (
                vec![AuthScope::SandboxUseReadOnly(sandbox_id)],
                vec![AuthScope::SandboxUseAll(sandbox_id)],
            ),
            SandboxAgentMode::Write => (
                vec![AuthScope::SandboxUseAll(sandbox_id)],
                vec![AuthScope::SandboxUseReadOnly(sandbox_id)],
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
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .agent_role(*agent_role)
                        .name(name.clone());
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
}

impl NewAgent {
    pub fn builder() -> NewAgentBuilder {
        let mut builder = NewAgentBuilder::default();
        builder.id(AgentId::new());
        builder
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
            }],
        )
    }
}
