use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;
use sandbox::instance_client::{ExportedFile, ExportedSkill, InitializeResponse};
use sandbox::{SandboxMode, SandboxSpecs};

use crate::primitives::*;

/// Many readers; at most one writer. Enforced in `attach_agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxAgentMode {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    Provisioning,
    Initializing,
    Ready,
    /// Pod/process deleted but project state (PVC / local dir) survives.
    Suspended,
    /// Reason persisted as `last_error` and `ProvisioningFailed` event.
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
        project_id: ProjectId,
        name: String,
        specs: SandboxSpecs,
        mode: SandboxMode,
        mount_path: String,
        #[serde(default)]
        workflow_id: Option<WorkflowDefinitionId>,
    },
    StateChanged {
        state: SandboxState,
    },
    /// Result of the sandbox-server's `/initialize` call: the
    /// inside-sandbox working directory plus any repo-mode exports
    /// (CLAUDE.md / `.claude/skills/...` / `.claude/commands/*.md`).
    /// Emitted exactly once per sandbox at the Ready transition,
    /// regardless of mode.
    RuntimePopulated {
        cwd: String,
        exported_system_prompt: Option<ExportedFile>,
        exported_skills: Vec<ExportedSkill>,
    },
    /// `step` (e.g. `create_sandbox`, `wait_ready`, `initialize`) groups
    /// failures in observability without parsing `reason`.
    ProvisioningFailed {
        step: String,
        reason: String,
    },
    /// Captures upgrade/downgrade if previously attached.
    AgentAttached {
        agent_id: AgentId,
        mode: SandboxAgentMode,
    },
    AgentDetached {
        agent_id: AgentId,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Sandbox {
    pub id: SandboxId,
    pub project_id: ProjectId,
    pub name: String,
    pub specs: SandboxSpecs,
    pub mode: SandboxMode,
    /// Cached at creation from the admin client.
    pub mount_path: String,
    /// Inside-sandbox working directory recorded from `/initialize`.
    /// Empty string until the sandbox reaches Ready (i.e. before
    /// `RuntimePopulated` fires); never read by the notification path
    /// because attach is gated on `state == Ready`.
    #[builder(default)]
    pub cwd: String,
    pub state: SandboxState,
    /// Set by `errored()`; cleared on transition out of `Errored`.
    pub last_error: Option<String>,
    pub exported_system_prompt: Option<ExportedFile>,
    pub exported_skills: Vec<ExportedSkill>,
    /// At most one Write entry, enforced by `attach_agent`.
    pub attached_agents: Vec<(AgentId, SandboxAgentMode)>,
    /// `Some` for sandboxes brought up by a workflow run; the executor
    /// reuses these across runs of the same definition.
    #[builder(default)]
    pub workflow_id: Option<WorkflowDefinitionId>,
    events: EntityEvents<SandboxEvent>,
}

impl Sandbox {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    /// K8s CR name / local sandbox dir name. Derived from id for k8s-safe uniqueness.
    pub fn resource_name(&self) -> String {
        format!("sb-{}", self.id)
    }

    /// `(kind, scope_label)` derived from the bootstrap mode. Used to
    /// render the `<sandbox>` notification — `kind` picks the body
    /// template, `scope_label` is shown in the header.
    pub fn kind_and_scope(&self) -> (&'static str, Option<String>) {
        match &self.mode {
            SandboxMode::Scratch => ("scratch", None),
            SandboxMode::Repo { repo_url, .. } => {
                let label = repo_url
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .map(|n| n.strip_suffix(".git").unwrap_or(n))
                    .filter(|n| !n.is_empty())
                    .map(|n| format!("repo \"{n}\""));
                ("repo", label)
            }
        }
    }

    /// Read-side fallback for `Skills::find_by_name` over `.claude/commands/*.md`.
    pub fn find_skill(&self, name: &str) -> Option<String> {
        self.exported_skills
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.content.clone())
    }

    pub(super) fn provisioning(&mut self) -> Idempotent<()> {
        self.transition_in_event(SandboxState::Provisioning)
    }

    pub(super) fn initializing(&mut self) -> Idempotent<()> {
        self.transition_in_event(SandboxState::Initializing)
    }

    pub(super) fn suspended(&mut self) -> Idempotent<()> {
        self.transition_in_event(SandboxState::Suspended)
    }

    /// Always pushes `ProvisioningFailed`; `AlreadyApplied` only when
    /// already `Errored` with the same reason.
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
        // Emit StateChanged for consumers that only hydrate from it.
        self.events.push(SandboxEvent::StateChanged {
            state: SandboxState::Errored,
        });
        Idempotent::Executed(())
    }

    /// Clears `last_error` on transition out of `Errored`.
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

    /// Transitions to `Ready` and emits `RuntimePopulated` with the
    /// `cwd` reported by `/initialize`. Repo-mode exports are folded
    /// into the same event so a single emission captures everything
    /// the runtime call produced.
    pub(super) fn initialized(&mut self, response: &InitializeResponse) -> Idempotent<()> {
        let state_changed = self.transition_in_event(SandboxState::Ready).did_execute();

        let has_exports =
            response.exported_system_prompt.is_some() || !response.exported_skills.is_empty();
        // Only repo-mode sandboxes ship `.claude/...` skills via the
        // sandbox server's `scan_skills`. Scratch sandboxes never
        // contain skills, so we drop any spurious exports there.
        let keep_exports = matches!(self.mode, SandboxMode::Repo { .. }) && has_exports;
        // RuntimePopulated is emit-once at the Ready transition;
        // `self.cwd` is empty exactly until that event fires.
        let needs_populate = self.cwd.is_empty();

        if needs_populate {
            let exported_system_prompt = if keep_exports {
                response.exported_system_prompt.clone()
            } else {
                None
            };
            let exported_skills = if keep_exports {
                response.exported_skills.clone()
            } else {
                Vec::new()
            };
            self.cwd = response.cwd.clone();
            self.exported_system_prompt = exported_system_prompt.clone();
            self.exported_skills = exported_skills.clone();
            self.events.push(SandboxEvent::RuntimePopulated {
                cwd: response.cwd.clone(),
                exported_system_prompt,
                exported_skills,
            });
        }

        if state_changed || needs_populate {
            Idempotent::Executed(())
        } else {
            Idempotent::AlreadyApplied
        }
    }

    /// Project mismatch → `WrongProject`. Write while another writer
    /// exists → `WriteSlotTaken`. Same mode is idempotent; different mode
    /// is in-place upgrade/downgrade.
    pub(super) fn attach_agent(
        &mut self,
        agent_id: AgentId,
        project_id: ProjectId,
        mode: SandboxAgentMode,
    ) -> Result<Idempotent<()>, super::error::SandboxError> {
        if self.project_id != project_id {
            return Err(super::error::SandboxError::WrongProject {
                expected: project_id,
                actual: self.project_id,
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

    /// Idempotent if not attached.
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

        // ProvisioningFailed sets; non-Errored StateChanged clears.
        let mut last_error: Option<String> = None;

        for event in events.iter_all() {
            match event {
                SandboxEvent::Initialized {
                    id,
                    project_id,
                    name,
                    specs,
                    mode,
                    mount_path,
                    workflow_id,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .name(name.clone())
                        .specs(specs.clone())
                        .mode(mode.clone())
                        .mount_path(mount_path.clone())
                        .state(SandboxState::Provisioning)
                        .exported_system_prompt(None)
                        .exported_skills(Vec::new())
                        .workflow_id(*workflow_id);
                }
                SandboxEvent::StateChanged { state } => {
                    if *state != SandboxState::Errored {
                        last_error = None;
                    }
                    builder = builder.state(*state);
                }
                SandboxEvent::RuntimePopulated {
                    cwd,
                    exported_system_prompt,
                    exported_skills,
                } => {
                    builder = builder
                        .cwd(cwd.clone())
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
    pub(super) project_id: ProjectId,
    #[builder(setter(into))]
    pub(super) name: String,
    pub(super) specs: SandboxSpecs,
    pub(super) mode: SandboxMode,
    #[builder(default, setter(into))]
    pub(super) mount_path: String,
    #[builder(default)]
    pub(super) workflow_id: Option<WorkflowDefinitionId>,
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
                project_id: self.project_id,
                name: self.name,
                specs: self.specs,
                mode: self.mode,
                mount_path: self.mount_path,
                workflow_id: self.workflow_id,
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
            dev_agent_token: None,
        }
    }

    fn new_sandbox() -> Sandbox {
        sandbox_in_project(ProjectId::new())
    }

    fn sandbox_in_project(project_id: ProjectId) -> Sandbox {
        let new = NewSandbox::builder()
            .id(SandboxId::new())
            .project_id(project_id)
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

    #[test]
    fn attach_agent_records_new_reader() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let agent = AgentId::new();

        let res = sb
            .attach_agent(agent, project, SandboxAgentMode::Read)
            .expect("attach");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Read)]);
    }

    #[test]
    fn attach_agent_records_new_writer() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let agent = AgentId::new();

        let res = sb
            .attach_agent(agent, project, SandboxAgentMode::Write)
            .expect("attach");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Write)]);
    }

    #[test]
    fn attach_agent_same_mode_is_idempotent() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let agent = AgentId::new();

        let _ = sb
            .attach_agent(agent, project, SandboxAgentMode::Read)
            .unwrap();
        let res = sb
            .attach_agent(agent, project, SandboxAgentMode::Read)
            .expect("re-attach");
        assert!(!res.did_execute(), "second attach must be AlreadyApplied");
    }

    #[test]
    fn attach_agent_upgrades_read_to_write_when_slot_free() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let agent = AgentId::new();

        let _ = sb
            .attach_agent(agent, project, SandboxAgentMode::Read)
            .unwrap();
        let res = sb
            .attach_agent(agent, project, SandboxAgentMode::Write)
            .expect("upgrade");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Write)]);
    }

    #[test]
    fn attach_agent_downgrades_write_to_read() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let agent = AgentId::new();

        let _ = sb
            .attach_agent(agent, project, SandboxAgentMode::Write)
            .unwrap();
        let res = sb
            .attach_agent(agent, project, SandboxAgentMode::Read)
            .expect("downgrade");
        assert!(res.did_execute());
        assert_eq!(sb.attached_agents, vec![(agent, SandboxAgentMode::Read)]);
    }

    #[test]
    fn attach_agent_allows_multiple_readers() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let a = AgentId::new();
        let b = AgentId::new();
        let c = AgentId::new();

        let _ = sb.attach_agent(a, project, SandboxAgentMode::Read).unwrap();
        let _ = sb.attach_agent(b, project, SandboxAgentMode::Read).unwrap();
        let _ = sb.attach_agent(c, project, SandboxAgentMode::Read).unwrap();
        assert_eq!(sb.attached_agents.len(), 3);
        assert!(sb
            .attached_agents
            .iter()
            .all(|(_, m)| *m == SandboxAgentMode::Read));
    }

    #[test]
    fn attach_agent_rejects_second_writer() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let writer_a = AgentId::new();
        let writer_b = AgentId::new();

        let _ = sb
            .attach_agent(writer_a, project, SandboxAgentMode::Write)
            .unwrap();
        // Map Ok to () for Debug since Idempotent doesn't derive Debug.
        let err = sb
            .attach_agent(writer_b, project, SandboxAgentMode::Write)
            .map(|_| ())
            .expect_err("second writer must be rejected");
        match err {
            super::super::error::SandboxError::WriteSlotTaken { current_writer } => {
                assert_eq!(current_writer, writer_a);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            sb.attached_agents,
            vec![(writer_a, SandboxAgentMode::Write)]
        );
    }

    #[test]
    fn attach_agent_allows_writer_with_existing_readers() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let r1 = AgentId::new();
        let r2 = AgentId::new();
        let w = AgentId::new();

        let _ = sb
            .attach_agent(r1, project, SandboxAgentMode::Read)
            .unwrap();
        let _ = sb
            .attach_agent(r2, project, SandboxAgentMode::Read)
            .unwrap();
        let res = sb
            .attach_agent(w, project, SandboxAgentMode::Write)
            .expect("writer ok with existing readers");
        assert!(res.did_execute());
    }

    #[test]
    fn attach_agent_rejects_wrong_project() {
        let owning_ws = ProjectId::new();
        let other_ws = ProjectId::new();
        let mut sb = sandbox_in_project(owning_ws);

        let err = sb
            .attach_agent(AgentId::new(), other_ws, SandboxAgentMode::Read)
            .map(|_| ())
            .expect_err("wrong project");
        match err {
            super::super::error::SandboxError::WrongProject { expected, actual } => {
                assert_eq!(expected, other_ws);
                assert_eq!(actual, owning_ws);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn detach_agent_removes_attachment() {
        let mut sb = new_sandbox();
        let project = sb.project_id;
        let agent = AgentId::new();
        let _ = sb
            .attach_agent(agent, project, SandboxAgentMode::Write)
            .unwrap();

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
        let sandbox_id = SandboxId::new();
        let project_id = ProjectId::new();
        let a = AgentId::new();
        let b = AgentId::new();

        let events = EntityEvents::init(
            sandbox_id,
            [
                SandboxEvent::Initialized {
                    id: sandbox_id,
                    project_id,
                    name: "test-sandbox".into(),
                    specs: test_specs(),
                    mode: SandboxMode::Scratch,
                    mount_path: "/workspace".into(),
                    workflow_id: None,
                },
                SandboxEvent::AgentAttached {
                    agent_id: a,
                    mode: SandboxAgentMode::Read,
                },
                SandboxEvent::AgentAttached {
                    agent_id: b,
                    mode: SandboxAgentMode::Write,
                },
                SandboxEvent::AgentDetached { agent_id: b },
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
        let project = sb.project_id;
        let a = AgentId::new();
        let b = AgentId::new();

        let _ = sb
            .attach_agent(a, project, SandboxAgentMode::Write)
            .unwrap();
        let _ = sb.detach_agent(a);
        let res = sb
            .attach_agent(b, project, SandboxAgentMode::Write)
            .expect("writer slot should be free after detach");
        assert!(res.did_execute());
    }
}
