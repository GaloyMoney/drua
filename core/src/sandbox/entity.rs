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
    /// Reason for the most recent failed provisioning step, set by
    /// [`Self::errored`] and rendered in the UI when `state == Errored`.
    /// Cleared back to `None` when the sandbox transitions out of
    /// `Errored` (e.g. via `provisioning()` on restart).
    #[builder(default)]
    pub last_error: Option<String>,
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
        self.events.push(SandboxEvent::ProvisioningFailed {
            step,
            reason,
        });
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
            }
        }

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
    fn provisioning_after_errored_clears_last_error() {
        let mut sb = new_sandbox();
        let _ = sb.errored("create_sandbox", "boom");
        let _ = sb.provisioning();
        assert_eq!(sb.state, SandboxState::Provisioning);
        assert!(sb.last_error.is_none());
    }
}
