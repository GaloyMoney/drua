use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::library::GitFileHash;
use crate::primitives::*;

use super::definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkflowDefinitionId")]
pub enum WorkflowDefinitionEvent {
    Initialized {
        id: WorkflowDefinitionId,
        workspace_id: WorkspaceId,
        #[serde(default)]
        workspace_name: Option<String>,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
        #[serde(default)]
        sandboxes: Vec<WorkflowSandboxDecl>,
        /// On-disk path before sync canonicalisation; the
        /// `WriteToRuntime` job uses it to remove the old file.
        #[serde(default)]
        original_path: Option<String>,
    },
    Updated {
        name: Option<String>,
        description: Option<String>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        #[serde(default)]
        sandboxes: Option<Vec<WorkflowSandboxDecl>>,
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
    #[builder(default)]
    pub sandboxes: Vec<WorkflowSandboxDecl>,
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

    pub(crate) fn as_runtime_file(&self) -> crate::library::UpstreamOp {
        crate::library::UpstreamOp::Synced(Box::new(
            <Self as crate::library::LibrarySynced>::to_synced_file(self),
        ))
    }

    /// Computed (not stored) so it matches what `WriteToRuntime` writes;
    /// otherwise reverse-sync drifts and re-emits commits in a loop
    /// (mirrors `Skill::file_hash`, drua commit f6dd821).
    pub(crate) fn file_hash(&self) -> GitFileHash {
        self.as_runtime_file().file_hash()
    }

    /// Webhook secrets stay DB-only — the splice below replays the
    /// existing one rather than letting the file overwrite it.
    pub fn update_from_library(
        &mut self,
        name: Option<String>,
        description: Option<Option<String>>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        sandboxes: Option<Vec<WorkflowSandboxDecl>>,
        incoming_file_hash: GitFileHash,
    ) -> Idempotent<()> {
        if self.file_hash() == incoming_file_hash {
            return Idempotent::AlreadyApplied;
        }

        if let Some(ref n) = name {
            self.name = n.clone();
        }
        if let Some(ref d) = description {
            self.description = d.clone();
        }
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
        if let Some(ref s) = sandboxes {
            self.sandboxes = s.clone();
        }

        self.events.push(WorkflowDefinitionEvent::Updated {
            name,
            description: description.flatten(),
            trigger: merged_trigger,
            steps,
            sandboxes,
        });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for WorkflowDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkflowDefinition: {}, name: {}", self.id, self.name)
    }
}

impl crate::library::LibrarySynced for WorkflowDefinition {
    type Event = WorkflowDefinitionEvent;
    const DOC_TYPE: crate::library::DocType = crate::library::DocType::Workflow;

    fn is_content_event(ev: &WorkflowDefinitionEvent) -> bool {
        matches!(
            ev,
            WorkflowDefinitionEvent::Initialized { .. } | WorkflowDefinitionEvent::Updated { .. }
        )
    }

    fn workspace(&self) -> Option<(WorkspaceId, &str)> {
        self.workspace_name
            .as_deref()
            .map(|n| (self.workspace_id, n))
    }

    fn id(&self) -> uuid::Uuid {
        self.id.into()
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .unwrap_or_else(chrono::Utc::now)
    }

    fn updated_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_last_modified_at()
            .or_else(|| self.events.entity_first_persisted_at())
            .unwrap_or_else(chrono::Utc::now)
    }

    fn original_path(&self) -> Option<&str> {
        self.original_path.as_deref()
    }

    fn index_body(&self) -> &str {
        self.description.as_deref().unwrap_or("")
    }

    fn render(&self) -> String {
        crate::library::render_workflow_yaml(
            self.id,
            &self.name,
            self.description.as_deref(),
            &self.trigger,
            &self.steps,
            &self.sandboxes,
            &<Self as crate::library::LibrarySynced>::created_at(self).to_rfc3339(),
            &<Self as crate::library::LibrarySynced>::updated_at(self).to_rfc3339(),
        )
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
                    sandboxes,
                    original_path,
                    ..
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .workspace_name(workspace_name.clone())
                        .name(name.clone())
                        .description(description.clone())
                        .trigger(trigger.clone())
                        .steps(steps.clone())
                        .sandboxes(sandboxes.clone())
                        .original_path(original_path.clone());
                }
                WorkflowDefinitionEvent::Updated {
                    name,
                    description,
                    trigger,
                    steps,
                    sandboxes,
                    ..
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
                    if let Some(s) = sandboxes {
                        builder = builder.sandboxes(s.clone());
                    }
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
    #[builder(default)]
    pub(super) sandboxes: Vec<WorkflowSandboxDecl>,
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
                sandboxes: self.sandboxes,
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
            sandbox_mode: None,
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
    }
}
