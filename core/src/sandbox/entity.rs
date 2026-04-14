use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;
use sandbox::instance_client::{ExportedFile, ExportedSkill, InitializeResponse};
use sandbox::{SandboxMode, SandboxSpecs};

use crate::primitives::*;

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
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Sandbox {
    pub id: SandboxId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub specs: SandboxSpecs,
    pub mode: SandboxMode,
    #[builder(default = "SandboxState::Provisioning")]
    pub state: SandboxState,
    #[builder(default)]
    pub exported_system_prompt: Option<ExportedFile>,
    #[builder(default)]
    pub exported_skills: Vec<ExportedSkill>,
    events: EntityEvents<SandboxEvent>,
}

impl Sandbox {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    /// Move to `next_state` if not already there. Returns
    /// [`Idempotent::AlreadyApplied`] if the entity is already in that state.
    pub(super) fn transition_to(&mut self, next_state: SandboxState) -> Idempotent<()> {
        if self.state == next_state {
            return Idempotent::AlreadyApplied;
        }
        self.state = next_state;
        self.events
            .push(SandboxEvent::StateChanged { state: next_state });
        Idempotent::Executed(())
    }

    /// Apply the result of a successful `/initialize` call: transition to
    /// [`SandboxState::Ready`] and, when the sandbox is in repo mode and
    /// the response actually carried any exports, persist an
    /// [`SandboxEvent::ExportsUpdated`] event.
    pub(super) fn initialized(&mut self, response: &InitializeResponse) -> Idempotent<()> {
        let state_changed = self.transition_to(SandboxState::Ready).did_execute();

        let has_exports = response.exported_system_prompt.is_some()
            || !response.exported_skills.is_empty();
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
}

impl core::fmt::Display for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sandbox: {}, name: {}, state: {:?}", self.id, self.name, self.state)
    }
}

impl TryFromEvents<SandboxEvent> for Sandbox {
    fn try_from_events(events: EntityEvents<SandboxEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = SandboxBuilder::default();

        for event in events.iter_all() {
            match event {
                SandboxEvent::Initialized {
                    id,
                    workspace_id,
                    name,
                    specs,
                    mode,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .name(name.clone())
                        .specs(specs.clone())
                        .mode(mode.clone())
                        .state(SandboxState::Provisioning);
                }
                SandboxEvent::StateChanged { state } => {
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
            }
        }

        builder.events(events).build()
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
        let new = NewSandbox::builder()
            .id(SandboxId::new())
            .workspace_id(WorkspaceId::new())
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
    fn transition_to_advances_state() {
        let mut sb = new_sandbox();
        let res = sb.transition_to(SandboxState::Initializing);
        assert!(res.did_execute());
        assert_eq!(sb.state, SandboxState::Initializing);
    }

    #[test]
    fn transition_to_same_state_is_idempotent() {
        let mut sb = new_sandbox();
        let res = sb.transition_to(SandboxState::Provisioning);
        assert!(!res.did_execute());
    }
}
