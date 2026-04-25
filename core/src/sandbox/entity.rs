use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;
use sandbox::instance_client::{ExportedFile, ExportedSkill, InitializeResponse};
use sandbox::{SandboxMode, SandboxSpecs};

use crate::primitives::*;

/// How an agent is attached to a sandbox.
///
/// Multiple agents may attach in [`SandboxAgentMode::Read`]; at most one
/// agent may attach in [`SandboxAgentMode::Write`] at a time. The entity
/// enforces this invariant in [`Sandbox::attach_agent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxAgentMode {
    Read,
    Write,
}

/// Tracked lifecycle state of the remote sandbox (k8s pod or local process).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// Admin client has been asked to create the sandbox; the underlying
    /// container/process isn't ready yet.
    Provisioning,
    /// The sandbox container/process is up and the `/initialize` endpoint
    /// is being invoked (clone repo, write github token, etc.).
    Initializing,
    /// `/initialize` has run successfully — the sandbox is ready to accept
    /// `/execute` calls.
    Ready,
    /// The sandbox has been suspended via the admin client — the underlying
    /// pod/process has been deleted, but workspace state survives (k8s: the
    /// PVC is retained; local: the `.sandboxes/<name>/` directory is left
    /// in place). Recreating with the same name resumes from that state.
    Suspended,
    /// A step in the provisioning / restart lifecycle failed. The failure
    /// reason is recorded on the entity as `last_error` and emitted as a
    /// [`SandboxEvent::ProvisioningFailed`]. Calling `restart()` clears
    /// this and re-runs the lifecycle.
    Errored,
}

impl core::fmt::Display for SandboxState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            SandboxState::Provisioning => "provisioning",
            SandboxState::Initializing => "initializing",
            SandboxState::Ready => "ready",
            SandboxState::Suspended => "suspended",
            SandboxState::Errored => "errored",
        };
        f.write_str(s)
    }
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SandboxId")]
pub enum SandboxEvent {
    Initialized {
        id: SandboxId,
        workspace_id: WorkspaceId,
        name: String,
        specs: SandboxSpecs,
        mode: SandboxMode,
        mount_path: String,
    },
    StateChanged {
        state: SandboxState,
    },
    /// Captured the result of a successful `/initialize` call when the
    /// sandbox is in repo mode and the cloned repo exposed a CLAUDE.md
    /// system prompt and/or `.claude/commands/*.md` skills.
    ExportsUpdated {
        exported_system_prompt: Option<ExportedFile>,
        exported_skills: Vec<ExportedSkill>,
    },
    /// A step in the provisioning / restart lifecycle failed. `step` names
    /// the failing step (`create_sandbox`, `wait_ready`, `initialize`, …)
    /// so we can group failures in observability without parsing `reason`.
    ProvisioningFailed {
        step: String,
        reason: String,
    },
    /// An agent is now attached to this sandbox in `mode`. If the agent was
    /// previously attached in a different mode, this event captures the new
    /// state (upgrade/downgrade).
    AgentAttached {
        agent_id: AgentId,
        mode: SandboxAgentMode,
    },
    /// An agent that was previously attached has been detached.
    AgentDetached {
        agent_id: AgentId,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Sandbox {
    pub id: SandboxId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub specs: SandboxSpecs,
    pub mode: SandboxMode,
    /// Absolute path to the workspace directory inside the sandbox,
    /// cached at creation time from the admin client.
    pub mount_path: String,
    pub state: SandboxState,
    /// Reason for the most recent failed provisioning step, set by
    /// [`Self::errored`] and rendered in the UI when `state == Errored`.
    /// Cleared back to `None` when the sandbox transitions out of
    /// `Errored` (e.g. via `provisioning()` on restart).
    pub last_error: Option<String>,
    pub exported_system_prompt: Option<ExportedFile>,
    pub exported_skills: Vec<ExportedSkill>,
    /// Agents currently attached to this sandbox. At most one entry may
    /// have [`SandboxAgentMode::Write`] (enforced by [`Self::attach_agent`]).
    pub attached_agents: Vec<(AgentId, SandboxAgentMode)>,
    events: EntityEvents<SandboxEvent>,
}

impl Sandbox {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    /// Stable identifier used for the underlying admin-client resource —
    /// the K8s Sandbox CR name in `K8sAdminClient` and the local sandbox
    /// directory name in `LocalAdminClient`. Derived from the entity id so
    /// it's globally unique and obeys k8s name constraints, regardless of
    /// the user-provided display `name`.
    pub fn resource_name(&self) -> String {
        format!("sb-{}", self.id)
    }

    /// Look up an exported skill by name and return its body. Exported
    /// skills are populated by `/initialize` from the cloned repo's
    /// `.claude/commands/*.md`; this is the read-side counterpart used
    /// by [`Skills::find_by_name`](super::super::skill::Skills::find_by_name)
    /// to fall back to in-sandbox skills when no DB-registered match
    /// exists.
    pub fn find_skill(&self, name: &str) -> Option<String> {
        self.exported_skills
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.content.clone())
    }

    /// Idempotent transition to [`SandboxState::Provisioning`]. Used when
    /// `restart()` re-runs the lifecycle from a `Suspended` or `Errored`
    /// sandbox; clears `last_error` so a stale failure isn't shown after
    /// the next attempt succeeds.
    pub(super) fn provisioning(&mut self) -> Idempotent<()> {
        self.transition_in_event(SandboxState::Provisioning)
    }

    /// Idempotent transition to [`SandboxState::Initializing`].
    pub(super) fn initializing(&mut self) -> Idempotent<()> {
        self.transition_in_event(SandboxState::Initializing)
    }

    /// Idempotent transition to [`SandboxState::Suspended`].
    pub(super) fn suspended(&mut self) -> Idempotent<()> {
        self.transition_in_event(SandboxState::Suspended)
    }

    /// Mark the sandbox as failed at `step` with `reason`. Always pushes a
    /// [`SandboxEvent::ProvisioningFailed`] (so we have a record of every
    /// attempt) and idempotently transitions to [`SandboxState::Errored`].
    /// Returns [`Idempotent::AlreadyApplied`] only if the entity is already
    /// `Errored` *and* the reason hasn't changed.
    pub(super) fn errored(
        &mut self,
        step: impl Into<String>,
        reason: impl Into<String>,
    ) -> Idempotent<()> {
        let step = step.into();
        let reason = reason.into();
        let already_errored_with_same_reason =
            self.state == SandboxState::Errored && self.last_error.as_deref() == Some(&reason);
        if already_errored_with_same_reason {
            return Idempotent::AlreadyApplied;
        }
        self.state = SandboxState::Errored;
        self.last_error = Some(reason.clone());
        self.events
            .push(SandboxEvent::ProvisioningFailed { step, reason });
        // Also emit a state change so existing consumers that only
        // hydrate from StateChanged keep working.
        self.events.push(SandboxEvent::StateChanged {
            state: SandboxState::Errored,
        });
        Idempotent::Executed(())
    }

    /// Shared body for the success-path transitions above. Pushes a
    /// `StateChanged` event when the state actually changes; clears
    /// `last_error` whenever we move *out* of `Errored` so the UI doesn't
    /// keep showing a stale failure after a successful retry.
    fn transition_in_event(&mut self, next_state: SandboxState) -> Idempotent<()> {
        if self.state == next_state {
            return Idempotent::AlreadyApplied;
        }
        self.state = next_state;
        self.last_error = None;
        self.events
            .push(SandboxEvent::StateChanged { state: next_state });
        Idempotent::Executed(())
    }

    /// Apply the result of a successful `/initialize` call: transition to
    /// [`SandboxState::Ready`] and, when the sandbox is in repo mode and
    /// the response actually carried any exports, persist an
    /// [`SandboxEvent::ExportsUpdated`] event.
    pub(super) fn initialized(&mut self, response: &InitializeResponse) -> Idempotent<()> {
        let state_changed = self.transition_in_event(SandboxState::Ready).did_execute();

        let has_exports =
            response.exported_system_prompt.is_some() || !response.exported_skills.is_empty();
        let push_exports = matches!(self.mode, SandboxMode::Repo { .. }) && has_exports;

        if push_exports {
            self.exported_system_prompt = response.exported_system_prompt.clone();
            self.exported_skills = response.exported_skills.clone();
            self.events.push(SandboxEvent::ExportsUpdated {
                exported_system_prompt: response.exported_system_prompt.clone(),
                exported_skills: response.exported_skills.clone(),
            });
        }

        if state_changed || push_exports {
            Idempotent::Executed(())
        } else {
            Idempotent::AlreadyApplied
        }
    }

    /// Attach `agent_id` in `mode`, enforcing the workspace and
    /// single-writer invariants.
    ///
    /// - The supplied `workspace_id` must match the sandbox's own
    ///   workspace — else returns
    ///   [`super::error::SandboxError::WrongWorkspace`].
    /// - Same agent already attached in the same mode → [`Idempotent::AlreadyApplied`].
    /// - Same agent already attached in a different mode → upgrade or downgrade
    ///   (a downgrade to Read always succeeds; an upgrade to Write only succeeds
    ///   when no other agent currently holds Write).
    /// - Attaching as Write while another agent already holds Write returns
    ///   [`super::error::SandboxError::WriteSlotTaken`].
    pub(super) fn attach_agent(
        &mut self,
        agent_id: AgentId,
        workspace_id: WorkspaceId,
        mode: SandboxAgentMode,
    ) -> Result<Idempotent<()>, super::error::SandboxError> {
        if self.workspace_id != workspace_id {
            return Err(super::error::SandboxError::WrongWorkspace {
                expected: workspace_id,
                actual: self.workspace_id,
            });
        }

        let current_mode = self
            .attached_agents
            .iter()
            .find(|(id, _)| *id == agent_id)
            .map(|(_, m)| *m);

        if current_mode == Some(mode) {
            return Ok(Idempotent::AlreadyApplied);
        }

        if mode == SandboxAgentMode::Write {
            if let Some((other, _)) = self
                .attached_agents
                .iter()
                .find(|(id, m)| *id != agent_id && *m == SandboxAgentMode::Write)
            {
                return Err(super::error::SandboxError::WriteSlotTaken {
                    current_writer: *other,
                });
            }
        }

        if let Some(entry) = self
            .attached_agents
            .iter_mut()
            .find(|(id, _)| *id == agent_id)
        {
            entry.1 = mode;
        } else {
            self.attached_agents.push((agent_id, mode));
        }
        self.events
            .push(SandboxEvent::AgentAttached { agent_id, mode });
        Ok(Idempotent::Executed(()))
    }

    /// Detach `agent_id`. Idempotent: no-op if the agent isn't attached.
    pub(super) fn detach_agent(&mut self, agent_id: AgentId) -> Idempotent<()> {
        let len_before = self.attached_agents.len();
        self.attached_agents.retain(|(id, _)| *id != agent_id);
        if self.attached_agents.len() == len_before {
            return Idempotent::AlreadyApplied;
        }
        self.events.push(SandboxEvent::AgentDetached { agent_id });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sandbox: {}, name: {}, state: {:?}",
            self.id, self.name, self.state
        )
    }
}

impl TryFromEvents<SandboxEvent> for Sandbox {
    fn try_from_events(events: EntityEvents<SandboxEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = SandboxBuilder::default();
        let mut attached_agents: Vec<(AgentId, SandboxAgentMode)> = Vec::new();

        // We accumulate `last_error` as we walk the events: a
        // `ProvisioningFailed` sets it; any subsequent successful
        // `StateChanged` (i.e. *out* of `Errored`) clears it. This mirrors
        // the live mutators in the entity.
        let mut last_error: Option<String> = None;

        for event in events.iter_all() {
            match event {
                SandboxEvent::Initialized {
                    id,
                    workspace_id,
                    name,
                    specs,
                    mode,
                    mount_path,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .name(name.clone())
                        .specs(specs.clone())
                        .mode(mode.clone())
                        .mount_path(mount_path.clone())
                        .state(SandboxState::Provisioning)
                        .exported_system_prompt(None)
                        .exported_skills(Vec::new());
                }
                SandboxEvent::StateChanged { state } => {
                    if *state != SandboxState::Errored {
                        last_error = None;
                    }
                    builder = builder.state(*state);
                }
                SandboxEvent::ExportsUpdated {
                    exported_system_prompt,
                    exported_skills,
                } => {
                    builder = builder
                        .exported_system_prompt(exported_system_prompt.clone())
                        .exported_skills(exported_skills.clone());
                }
                SandboxEvent::ProvisioningFailed { reason, .. } => {
                    last_error = Some(reason.clone());
                }
                SandboxEvent::AgentAttached { agent_id, mode } => {
                    if let Some(entry) = attached_agents
                        .iter_mut()
                        .find(|(id, _): &&mut (AgentId, SandboxAgentMode)| *id == *agent_id)
                    {
                        entry.1 = *mode;
                    } else {
                        attached_agents.push((*agent_id, *mode));
                    }
                }
                SandboxEvent::AgentDetached { agent_id } => {
                    attached_agents.retain(|(id, _)| *id != *agent_id);
                }
            }
        }
        builder = builder.attached_agents(attached_agents);

        builder.last_error(last_error).events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewSandbox {
    #[builder(setter(into))]
    pub(super) id: SandboxId,
    #[builder(setter(into))]
    pub(super) workspace_id: WorkspaceId,
    #[builder(setter(into))]
    pub(super) name: String,
    pub(super) specs: SandboxSpecs,
    pub(super) mode: SandboxMode,
    #[builder(default, setter(into))]
    pub(super) mount_path: String,
}

impl NewSandbox {
    pub fn builder() -> NewSandboxBuilder {
        let mut builder = NewSandboxBuilder::default();
        builder.id(SandboxId::new());
        builder
    }
}

impl IntoEvents<SandboxEvent> for NewSandbox {
    fn into_events(self) -> EntityEvents<SandboxEvent> {
        EntityEvents::init(
            self.id,
            [SandboxEvent::Initialized {
                id: self.id,
                workspace_id: self.workspace_id,
                name: self.name,
                specs: self.specs,
                mode: self.mode,
                mount_path: self.mount_path,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn test_specs() -> SandboxSpecs {
        SandboxSpecs {
            cpu: "100m".into(),
            memory: "128Mi".into(),
            disk_size: "1Gi".into(),
        }
    }

    fn new_sandbox() -> Sandbox {
        sandbox_in_workspace(WorkspaceId::new())
    }

    fn sandbox_in_workspace(workspace_id: WorkspaceId) -> Sandbox {
        let new = NewSandbox::builder()
            .id(SandboxId::new())
            .workspace_id(workspace_id)
            .name("test-sandbox")
            .specs(test_specs())
            .mode(SandboxMode::Scratch)
            .build()
            .unwrap();
        Sandbox::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn sandbox_starts_in_provisioning() {
        let sb = new_sandbox();
        assert_eq!(sb.state, SandboxState::Provisioning);
        assert_eq!(sb.name, "test-sandbox");
    }

    #[test]
    fn initializing_advances_state() {
        let mut sb = new_sandbox();
        let res = sb.initializing();
        assert!(res.did_execute());
        assert_eq!(sb.state, SandboxState::Initializing);
    }

    #[test]
    fn provisioning_from_provisioning_is_idempotent() {
        let mut sb = new_sandbox();
        let res = sb.provisioning();
        assert!(!res.did_execute());
    }

    #[test]
    fn errored_records_reason_and_state() {
        let mut sb = new_sandbox();
        let res = sb.errored("create_sandbox", "k8s api timeout");
        assert!(res.did_execute());
        assert_eq!(sb.state, SandboxState::Errored);
        assert_eq!(sb.last_error.as_deref(), Some("k8s api timeout"));
    }

    #[test]
    fn errored_with_same_reason_is_idempotent() {
        let mut sb = new_sandbox();
        let _ = sb.errored("create_sandbox", "boom");
        let res = sb.errored("create_sandbox", "boom");
        assert!(!res.did_execute());
    }

    // ── attach_agent / detach_agent ────────────────────────────────

    #[test]
    fn attach_agent_records_new_reader() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let agent = AgentId::new();

        let res = sb
            .attach_agent(agent, ws, SandboxAgentMode::Read)
            .expect("attach");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Read)]);
    }

    #[test]
    fn attach_agent_records_new_writer() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let agent = AgentId::new();

        let res = sb
            .attach_agent(agent, ws, SandboxAgentMode::Write)
            .expect("attach");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Write)]);
    }

    #[test]
    fn attach_agent_same_mode_is_idempotent() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let agent = AgentId::new();

        let _ = sb.attach_agent(agent, ws, SandboxAgentMode::Read).unwrap();
        let res = sb
            .attach_agent(agent, ws, SandboxAgentMode::Read)
            .expect("re-attach");
        assert!(!res.did_execute(), "second attach must be AlreadyApplied");
    }

    #[test]
    fn attach_agent_upgrades_read_to_write_when_slot_free() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let agent = AgentId::new();

        let _ = sb.attach_agent(agent, ws, SandboxAgentMode::Read).unwrap();
        let res = sb
            .attach_agent(agent, ws, SandboxAgentMode::Write)
            .expect("upgrade");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Write)]);
    }

    #[test]
    fn attach_agent_downgrades_write_to_read() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let agent = AgentId::new();

        let _ = sb.attach_agent(agent, ws, SandboxAgentMode::Write).unwrap();
        let res = sb
            .attach_agent(agent, ws, SandboxAgentMode::Read)
            .expect("downgrade");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Read)]);
    }

    #[test]
    fn attach_agent_allows_multiple_readers() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let a = AgentId::new();
        let b = AgentId::new();
        let c = AgentId::new();

        let _ = sb.attach_agent(a, ws, SandboxAgentMode::Read).unwrap();
        let _ = sb.attach_agent(b, ws, SandboxAgentMode::Read).unwrap();
        let _ = sb.attach_agent(c, ws, SandboxAgentMode::Read).unwrap();
        assert_eq!(sb.attached_agents.len(), 3);
        assert!(sb
            .attached_agents
            .iter()
            .all(|(_, m)| *m == SandboxAgentMode::Read));
    }

    #[test]
    fn attach_agent_rejects_second_writer() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let writer_a = AgentId::new();
        let writer_b = AgentId::new();

        let _ = sb
            .attach_agent(writer_a, ws, SandboxAgentMode::Write)
            .unwrap();
        // Map Ok to () so we can rely on the Debug impl for expect_err —
        // `Idempotent` doesn't derive Debug.
        let err = sb
            .attach_agent(writer_b, ws, SandboxAgentMode::Write)
            .map(|_| ())
            .expect_err("second writer must be rejected");
        match err {
            super::super::error::SandboxError::WriteSlotTaken { current_writer } => {
                assert_eq!(current_writer, writer_a);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // The attach list is unchanged.
        assert_eq!(
            sb.attached_agents,
            vec![(writer_a, SandboxAgentMode::Write)]
        );
    }

    #[test]
    fn attach_agent_allows_writer_with_existing_readers() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let r1 = AgentId::new();
        let r2 = AgentId::new();
        let w = AgentId::new();

        let _ = sb.attach_agent(r1, ws, SandboxAgentMode::Read).unwrap();
        let _ = sb.attach_agent(r2, ws, SandboxAgentMode::Read).unwrap();
        let res = sb
            .attach_agent(w, ws, SandboxAgentMode::Write)
            .expect("writer ok with existing readers");
        assert!(res.did_execute());
    }

    #[test]
    fn attach_agent_rejects_wrong_workspace() {
        let owning_ws = WorkspaceId::new();
        let other_ws = WorkspaceId::new();
        let mut sb = sandbox_in_workspace(owning_ws);

        let err = sb
            .attach_agent(AgentId::new(), other_ws, SandboxAgentMode::Read)
            .map(|_| ())
            .expect_err("wrong workspace");
        match err {
            super::super::error::SandboxError::WrongWorkspace { expected, actual } => {
                assert_eq!(expected, other_ws);
                assert_eq!(actual, owning_ws);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn detach_agent_removes_attachment() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let agent = AgentId::new();
        let _ = sb.attach_agent(agent, ws, SandboxAgentMode::Write).unwrap();

        let res = sb.detach_agent(agent);
        assert!(res.did_execute());
        assert!(sb.attached_agents.is_empty());
    }

    #[test]
    fn detach_unknown_agent_is_idempotent() {
        let mut sb = new_sandbox();
        let res = sb.detach_agent(AgentId::new());
        assert!(!res.did_execute());
    }

    #[test]
    fn provisioning_after_errored_clears_last_error() {
        let mut sb = new_sandbox();
        let _ = sb.errored("create_sandbox", "boom");
        let _ = sb.provisioning();
        assert_eq!(sb.state, SandboxState::Provisioning);
        assert!(sb.last_error.is_none());
    }

    #[test]
    fn hydration_replays_attach_detach_history() {
        // Synthesize a stream of events and verify try_from_events folds
        // them into the expected attached_agents state. Each AgentAttached
        // upserts the (agent_id, mode); AgentDetached removes the entry.
        let sandbox_id = SandboxId::new();
        let workspace_id = WorkspaceId::new();
        let a = AgentId::new();
        let b = AgentId::new();

        let events = EntityEvents::init(
            sandbox_id,
            [
                SandboxEvent::Initialized {
                    id: sandbox_id,
                    workspace_id,
                    name: "test-sandbox".into(),
                    specs: test_specs(),
                    mode: SandboxMode::Scratch,
                    mount_path: "/workspace".into(),
                },
                SandboxEvent::AgentAttached {
                    agent_id: a,
                    mode: SandboxAgentMode::Read,
                },
                SandboxEvent::AgentAttached {
                    agent_id: b,
                    mode: SandboxAgentMode::Write,
                },
                // b is detached; the writer slot is free again.
                SandboxEvent::AgentDetached { agent_id: b },
                // Now a upgrades to Write.
                SandboxEvent::AgentAttached {
                    agent_id: a,
                    mode: SandboxAgentMode::Write,
                },
            ],
        );

        let sb = Sandbox::try_from_events(events).unwrap();
        assert_eq!(sb.attached_agents, vec![(a, SandboxAgentMode::Write)]);
    }

    #[test]
    fn attach_agent_after_detach_can_reuse_writer_slot() {
        let mut sb = new_sandbox();
        let ws = sb.workspace_id;
        let a = AgentId::new();
        let b = AgentId::new();

        let _ = sb.attach_agent(a, ws, SandboxAgentMode::Write).unwrap();
        let _ = sb.detach_agent(a);
        let res = sb
            .attach_agent(b, ws, SandboxAgentMode::Write)
            .expect("writer slot should be free after detach");
        assert!(res.did_execute());
    }
}
