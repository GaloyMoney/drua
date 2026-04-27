use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

use super::definition::{WorkflowStepDef, WorkflowTrigger};

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkflowDefinitionId")]
pub enum WorkflowDefinitionEvent {
    Initialized {
        id: WorkflowDefinitionId,
        workspace_id: WorkspaceId,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct WorkflowDefinition {
    pub id: WorkflowDefinitionId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    #[builder(default)]
    pub description: Option<String>,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStepDef>,
    events: EntityEvents<WorkflowDefinitionEvent>,
}

impl WorkflowDefinition {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
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
                    name,
                    description,
                    trigger,
                    steps,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .name(name.clone())
                        .description(description.clone())
                        .trigger(trigger.clone())
                        .steps(steps.clone());
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
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(default, setter(into, strip_option))]
    pub(super) description: Option<String>,
    pub(super) trigger: WorkflowTrigger,
    pub(super) steps: Vec<WorkflowStepDef>,
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
                name: self.name,
                description: self.description,
                trigger: self.trigger,
                steps: self.steps,
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

    #[test]
    fn workflow_definition_hydration() {
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

        let def = WorkflowDefinition::try_from_events(new.into_events()).unwrap();
        assert_eq!(def.name, "test-flow");
        assert_eq!(def.steps.len(), 1);
        assert!(matches!(def.trigger, WorkflowTrigger::Webhook { .. }));
    }
}
