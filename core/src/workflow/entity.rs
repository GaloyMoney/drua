use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::library::GitFileHash;
use crate::primitives::*;

use super::definition::{WorkflowStepDef, WorkflowTrigger};

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkflowDefinitionId")]
pub enum WorkflowDefinitionEvent {
    Initialized {
        id: WorkflowDefinitionId,
        workspace_id: WorkspaceId,
        /// Cached workspace display name (set at creation, never
        /// mutated on rename — matches the skill/agent pattern).
        /// Required for the library forward-sync hook to render the
        /// file path. Optional in the event for backward compatibility
        /// with pre-sync workflows.
        #[serde(default)]
        workspace_name: Option<String>,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
        /// `Some` once the entity has been written to (or imported from)
        /// the git library; the post-persist hook stamps it on the
        /// follow-up `Updated` event when forward-syncing for the first
        /// time. Backfilled with `None` for legacy events.
        #[serde(default)]
        file_hash: Option<GitFileHash>,
        /// On reverse-sync, the file's path on disk before any
        /// canonicalisation by the sync job. `None` for entities
        /// created via the MCP tool.
        #[serde(default)]
        original_path: Option<String>,
    },
    /// Body or trigger changed (typically from a reverse-sync that
    /// detected a new file hash). Webhook secrets stay DB-only and are
    /// never overwritten by file edits — the sync path preserves them.
    Updated {
        name: Option<String>,
        description: Option<String>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        file_hash: GitFileHash,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct WorkflowDefinition {
    pub id: WorkflowDefinitionId,
    pub workspace_id: WorkspaceId,
    #[builder(default)]
    pub workspace_name: Option<String>,
    pub name: String,
    #[builder(default)]
    pub description: Option<String>,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStepDef>,
    /// Hash of the canonical YAML representation last persisted to /
    /// imported from the library. `None` until the first sync writes a
    /// file. Used to skip redundant forward-syncs and to detect
    /// reverse-sync diffs.
    #[builder(default)]
    pub file_hash: Option<GitFileHash>,
    /// On reverse-sync, the original on-disk path. The sync job uses
    /// this to remove the old file after writing the canonical one.
    #[builder(default)]
    pub(crate) original_path: Option<String>,
    events: EntityEvents<WorkflowDefinitionEvent>,
}

impl WorkflowDefinition {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn updated_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_last_modified_at()
            .or_else(|| self.events.entity_first_persisted_at())
            .expect("entity should have at least one persisted timestamp")
    }

    /// Build the canonical [`crate::library::RuntimeFile::Workflow`]
    /// for this entity. Uses the cached `workspace_name` so the
    /// post-persist hook doesn't need to do extra lookups.
    pub(crate) fn as_runtime_file(&self) -> crate::library::RuntimeFile {
        crate::library::RuntimeFile::for_workflow_with_original_path(
            self.id,
            Some(self.workspace_id),
            self.workspace_name.as_deref(),
            &self.name,
            self.description.as_deref(),
            self.trigger.clone(),
            self.steps.clone(),
            &self.created_at().to_rfc3339(),
            &self.updated_at().to_rfc3339(),
            self.original_path.clone(),
        )
    }

    /// Apply a reverse-sync update from the library. Compares the
    /// incoming file hash to the entity's stored hash and skips when
    /// they match. Webhook secrets are preserved — only the body of
    /// the trigger config (provider) and the steps can change via the
    /// file. Returns `Idempotent::AlreadyApplied` for a no-op.
    pub fn update_from_library(
        &mut self,
        name: Option<String>,
        description: Option<Option<String>>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        incoming_file_hash: GitFileHash,
    ) -> Idempotent<()> {
        if self.file_hash.as_ref() == Some(&incoming_file_hash) {
            return Idempotent::AlreadyApplied;
        }

        if let Some(ref n) = name {
            self.name = n.clone();
        }
        if let Some(ref d) = description {
            self.description = d.clone();
        }
        // Splice the existing webhook secret back in — file never carries it.
        let merged_trigger = trigger
            .as_ref()
            .map(|incoming| match (incoming, &self.trigger) {
                (
                    WorkflowTrigger::Webhook { provider, .. },
                    WorkflowTrigger::Webhook { secret, .. },
                ) => WorkflowTrigger::Webhook {
                    provider: provider.clone(),
                    secret: secret.clone(),
                },
                _ => incoming.clone(),
            });
        if let Some(t) = merged_trigger.clone() {
            self.trigger = t;
        }
        if let Some(ref s) = steps {
            self.steps = s.clone();
        }
        self.file_hash = Some(incoming_file_hash.clone());

        self.events.push(WorkflowDefinitionEvent::Updated {
            name,
            description: description.flatten(),
            trigger: merged_trigger,
            steps,
            file_hash: incoming_file_hash,
        });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for WorkflowDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkflowDefinition: {}, name: {}", self.id, self.name)
    }
}

impl TryFromEvents<WorkflowDefinitionEvent> for WorkflowDefinition {
    fn try_from_events(
        events: EntityEvents<WorkflowDefinitionEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = WorkflowDefinitionBuilder::default();

        for event in events.iter_all() {
            match event {
                WorkflowDefinitionEvent::Initialized {
                    id,
                    workspace_id,
                    workspace_name,
                    name,
                    description,
                    trigger,
                    steps,
                    file_hash,
                    original_path,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .workspace_name(workspace_name.clone())
                        .name(name.clone())
                        .description(description.clone())
                        .trigger(trigger.clone())
                        .steps(steps.clone())
                        .file_hash(file_hash.clone())
                        .original_path(original_path.clone());
                }
                WorkflowDefinitionEvent::Updated {
                    name,
                    description,
                    trigger,
                    steps,
                    file_hash,
                } => {
                    if let Some(n) = name {
                        builder = builder.name(n.clone());
                    }
                    if let Some(d) = description {
                        builder = builder.description(Some(d.clone()));
                    }
                    if let Some(t) = trigger {
                        builder = builder.trigger(t.clone());
                    }
                    if let Some(s) = steps {
                        builder = builder.steps(s.clone());
                    }
                    builder = builder.file_hash(Some(file_hash.clone()));
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct NewWorkflowDefinition {
    #[builder(setter(into))]
    pub(super) id: WorkflowDefinitionId,
    #[builder(setter(into))]
    pub(super) workspace_id: WorkspaceId,
    #[builder(default, setter(into, strip_option))]
    pub(super) workspace_name: Option<String>,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(default, setter(into, strip_option))]
    pub(super) description: Option<String>,
    pub(super) trigger: WorkflowTrigger,
    pub(super) steps: Vec<WorkflowStepDef>,
    /// On reverse-sync from the library, the on-disk path before
    /// canonicalisation. The sync job will rename / clean up afterwards.
    #[builder(default, setter(into, strip_option))]
    pub(super) original_path: Option<String>,
}

impl NewWorkflowDefinition {
    pub fn builder() -> NewWorkflowDefinitionBuilder {
        NewWorkflowDefinitionBuilder::default().id(WorkflowDefinitionId::new())
    }
}

impl IntoEvents<WorkflowDefinitionEvent> for NewWorkflowDefinition {
    fn into_events(self) -> EntityEvents<WorkflowDefinitionEvent> {
        EntityEvents::init(
            self.id,
            [WorkflowDefinitionEvent::Initialized {
                id: self.id,
                workspace_id: self.workspace_id,
                workspace_name: self.workspace_name,
                name: self.name,
                description: self.description,
                trigger: self.trigger,
                steps: self.steps,
                file_hash: None,
                original_path: self.original_path,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn sample_step() -> WorkflowStepDef {
        WorkflowStepDef::AgentStep {
            name: "investigate".to_string(),
            skill: "echo-test".to_string(),
            sandbox: None,
            timeout_seconds: Some(60),
        }
    }

    fn build() -> WorkflowDefinition {
        let new = NewWorkflowDefinition::builder()
            .workspace_id(WorkspaceId::new())
            .name("test-flow")
            .trigger(WorkflowTrigger::Webhook {
                provider: Some("honeycomb".into()),
                secret: "whsec_xxx".into(),
            })
            .steps(vec![sample_step()])
            .build()
            .unwrap();
        WorkflowDefinition::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn workflow_definition_hydration() {
        let def = build();
        assert_eq!(def.name, "test-flow");
        assert_eq!(def.steps.len(), 1);
        assert!(matches!(def.trigger, WorkflowTrigger::Webhook { .. }));
        // No forward-sync yet, so no file_hash stamped.
        assert!(def.file_hash.is_none());
    }

    #[test]
    fn update_from_library_preserves_secret() {
        let mut def = build();
        let h = GitFileHash::default();
        let result = def.update_from_library(
            None,
            None,
            Some(WorkflowTrigger::Webhook {
                provider: Some("honeycomb".into()),
                // Incoming "secret" — should be ignored, not propagated.
                secret: "".into(),
            }),
            None,
            h,
        );
        assert!(matches!(result, Idempotent::Executed(())));
        match &def.trigger {
            WorkflowTrigger::Webhook { secret, .. } => {
                assert_eq!(secret, "whsec_xxx");
            }
            _ => panic!("expected webhook"),
        }
    }
}
