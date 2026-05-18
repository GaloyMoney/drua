use derive_builder::Builder;
use drua_library::{GitFileHash, SearchableFields, WriteOp};
use llm::ModelChain;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;
use crate::skill::file::slugify;
use crate::workflow::WORKFLOW_DOC_TYPE;

use super::definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};
use super::yaml::{canonical_workflow_path, render_workflow_yaml};

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkflowDefinitionId")]
pub enum WorkflowDefinitionEvent {
    Initialized {
        id: WorkflowDefinitionId,
        project_id: ProjectId,
        #[serde(default)]
        project_name: Option<String>,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
        #[serde(default)]
        sandboxes: Vec<WorkflowSandboxDecl>,
        /// Per-step `model_chain` overrides this; both fall through to
        /// the role/config default when unset.
        #[serde(default)]
        model_chain: Option<ModelChain>,
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
        /// `Some(Some(_))` sets / replaces; `Some(None)` clears.
        /// `None` leaves the field untouched.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_chain: Option<Option<ModelChain>>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct WorkflowDefinition {
    pub id: WorkflowDefinitionId,
    pub project_id: ProjectId,
    #[builder(default)]
    pub project_name: Option<String>,
    pub name: String,
    #[builder(default)]
    pub description: Option<String>,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStepDef>,
    #[builder(default)]
    pub sandboxes: Vec<WorkflowSandboxDecl>,
    /// Per-step `model_chain` wins; both fall through to role/config.
    #[builder(default)]
    pub model_chain: Option<ModelChain>,
    #[builder(default)]
    pub(crate) original_path: Option<String>,
    events: EntityEvents<WorkflowDefinitionEvent>,
}

impl WorkflowDefinition {
    pub fn provider(&self) -> Option<String> {
        match &self.trigger {
            WorkflowTrigger::Webhook {
                provider: Some(p), ..
            } => Some(p.clone()),
            _ => None,
        }
    }

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

    pub fn canonical_yaml(&self) -> String {
        self.rendered()
    }

    /// Canonical on-disk content (YAML).
    pub(crate) fn rendered(&self) -> String {
        render_workflow_yaml(
            self.id,
            &self.name,
            self.description.as_deref(),
            &self.trigger,
            &self.steps,
            &self.sandboxes,
            self.model_chain.as_ref(),
            &self.created_at().to_rfc3339(),
            &self.updated_at().to_rfc3339(),
        )
    }

    /// Computed (not stored) so it matches what `WriteToRuntime` writes;
    /// otherwise reverse-sync drifts and re-emits commits in a loop
    /// (mirrors `Skill::file_hash`, drua commit f6dd821).
    pub(crate) fn file_hash(&self) -> GitFileHash {
        GitFileHash::new(self.rendered())
    }

    /// Webhook secrets stay DB-only — the splice below replays the
    /// existing one rather than letting the file overwrite it.
    #[allow(clippy::too_many_arguments)]
    pub fn update_from_library(
        &mut self,
        name: Option<String>,
        description: Option<Option<String>>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        sandboxes: Option<Vec<WorkflowSandboxDecl>>,
        model_chain: Option<Option<ModelChain>>,
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
                    WorkflowTrigger::Webhook {
                        provider,
                        condition,
                        ..
                    },
                    WorkflowTrigger::Webhook { secret, .. },
                ) => WorkflowTrigger::Webhook {
                    provider: provider.clone(),
                    secret: secret.clone(),
                    condition: condition.clone(),
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
        if let Some(mc) = &model_chain {
            self.model_chain = mc.clone();
        }

        self.events.push(WorkflowDefinitionEvent::Updated {
            name,
            description: description.flatten(),
            trigger: merged_trigger,
            steps,
            sandboxes,
            model_chain,
        });
        Idempotent::Executed(())
    }

    /// User-driven path (no file_hash compare; that's [`Self::update_from_library`]).
    /// Webhook secrets are preserved when only `provider` changes.
    /// Returns `AlreadyApplied` only when every input is `None`.
    pub fn update_content(
        &mut self,
        name: Option<String>,
        description: Option<Option<String>>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        sandboxes: Option<Vec<WorkflowSandboxDecl>>,
        model_chain: Option<Option<ModelChain>>,
    ) -> Idempotent<()> {
        if name.is_none()
            && description.is_none()
            && trigger.is_none()
            && steps.is_none()
            && sandboxes.is_none()
            && model_chain.is_none()
        {
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
                    WorkflowTrigger::Webhook {
                        provider,
                        condition,
                        ..
                    },
                    WorkflowTrigger::Webhook { secret, .. },
                ) => WorkflowTrigger::Webhook {
                    provider: provider.clone(),
                    secret: secret.clone(),
                    condition: condition.clone(),
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
        if let Some(mc) = &model_chain {
            self.model_chain = mc.clone();
        }

        self.events.push(WorkflowDefinitionEvent::Updated {
            name,
            description: description.flatten(),
            trigger: merged_trigger,
            steps,
            sandboxes,
            model_chain,
        });
        Idempotent::Executed(())
    }

    /// Precedence: step `model_chain` > workflow `model_chain` > None.
    pub fn resolve_step_chain(&self, step: &WorkflowStepDef) -> Option<ModelChain> {
        step.model_chain()
            .cloned()
            .or_else(|| self.model_chain.clone())
    }
}

impl core::fmt::Display for WorkflowDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkflowDefinition: {}, name: {}", self.id, self.name)
    }
}

impl drua_library::LibrarySynced for WorkflowDefinition {
    type Event = WorkflowDefinitionEvent;

    fn is_content_event(ev: &WorkflowDefinitionEvent) -> bool {
        matches!(
            ev,
            WorkflowDefinitionEvent::Initialized { .. } | WorkflowDefinitionEvent::Updated { .. }
        )
    }

    fn searchable_fields(&self) -> SearchableFields {
        let project_name = self.project_name.as_deref();
        SearchableFields {
            doc_id: self.id.into(),
            doc_type: WORKFLOW_DOC_TYPE,
            scope_id: Some(self.project_id.into()),
            scope_slug: project_name.map(str::to_string),
            name: self.name.clone(),
            path: Some(canonical_workflow_path(&self.name, project_name)),
            content: self.description.clone().unwrap_or_default(),
        }
    }

    fn write_op(&self) -> WriteOp {
        let canonical = canonical_workflow_path(&self.name, self.project_name.as_deref());
        let content = self.rendered().into_bytes();
        let id_uuid: uuid::Uuid = self.id.into();
        let message = format!(
            "workflow: {}-{}",
            slugify(&self.name),
            &id_uuid.to_string()[..8]
        );
        match self.original_path.as_deref() {
            Some(orig) if orig != canonical => WriteOp::WriteFileWithRename {
                old_path: orig.to_string(),
                new_path: canonical,
                content,
                message,
            },
            _ => WriteOp::WriteFile {
                path: canonical,
                content,
                message,
            },
        }
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
                    project_id,
                    project_name,
                    name,
                    description,
                    trigger,
                    steps,
                    sandboxes,
                    model_chain,
                    original_path,
                    ..
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .project_name(project_name.clone())
                        .name(name.clone())
                        .description(description.clone())
                        .trigger(trigger.clone())
                        .steps(steps.clone())
                        .sandboxes(sandboxes.clone())
                        .model_chain(model_chain.clone())
                        .original_path(original_path.clone());
                }
                WorkflowDefinitionEvent::Updated {
                    name,
                    description,
                    trigger,
                    steps,
                    sandboxes,
                    model_chain,
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
                    if let Some(mc) = model_chain {
                        builder = builder.model_chain(mc.clone());
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
    pub(super) project_id: ProjectId,
    #[builder(default, setter(into, strip_option))]
    pub(super) project_name: Option<String>,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(default, setter(into, strip_option))]
    pub(super) description: Option<String>,
    pub(super) trigger: WorkflowTrigger,
    pub(super) steps: Vec<WorkflowStepDef>,
    #[builder(default)]
    pub(super) sandboxes: Vec<WorkflowSandboxDecl>,
    #[builder(default)]
    pub(super) model_chain: Option<ModelChain>,
    #[builder(default, setter(into, strip_option))]
    pub(super) original_path: Option<String>,
}

impl NewWorkflowDefinition {
    pub fn builder() -> NewWorkflowDefinitionBuilder {
        NewWorkflowDefinitionBuilder::default().id(WorkflowDefinitionId::new())
    }

    pub fn provider(&self) -> Option<String> {
        match &self.trigger {
            WorkflowTrigger::Webhook {
                provider: Some(p), ..
            } => Some(p.clone()),
            _ => None,
        }
    }
}

impl IntoEvents<WorkflowDefinitionEvent> for NewWorkflowDefinition {
    fn into_events(self) -> EntityEvents<WorkflowDefinitionEvent> {
        EntityEvents::init(
            self.id,
            [WorkflowDefinitionEvent::Initialized {
                id: self.id,
                project_id: self.project_id,
                project_name: self.project_name,
                name: self.name,
                description: self.description,
                trigger: self.trigger,
                steps: self.steps,
                sandboxes: self.sandboxes,
                model_chain: self.model_chain,
                original_path: self.original_path,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::super::definition::default_output_schema;
    use super::*;

    fn sample_step() -> WorkflowStepDef {
        WorkflowStepDef::AgentStep {
            name: "investigate".to_string(),
            skill: "echo-test".to_string(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: Some(60),
            model_chain: None,
            output_schema: Box::new(default_output_schema()),
            condition: None,
        }
    }

    fn build() -> WorkflowDefinition {
        let new = NewWorkflowDefinition::builder()
            .project_id(ProjectId::new())
            .name("test-flow")
            .trigger(WorkflowTrigger::Webhook {
                provider: Some("honeycomb".into()),
                secret: "whsec_xxx".into(),
                condition: None,
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

    #[test]
    fn resolve_step_chain_step_overrides_workflow_overrides_default() {
        let step_chain = ModelChain::new("per-step");
        let workflow_chain = ModelChain::new("workflow-wide");

        let mut def = build();
        def.model_chain = Some(workflow_chain.clone());
        def.steps = vec![WorkflowStepDef::AgentStep {
            name: "s".into(),
            skill: "k".into(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: Some(step_chain.clone()),
            output_schema: Box::new(default_output_schema()),
            condition: None,
        }];
        assert_eq!(
            def.resolve_step_chain(&def.steps[0]).unwrap().primary.name,
            "per-step"
        );

        def.steps = vec![WorkflowStepDef::AgentStep {
            name: "s".into(),
            skill: "k".into(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: None,
            output_schema: Box::new(default_output_schema()),
            condition: None,
        }];
        assert_eq!(
            def.resolve_step_chain(&def.steps[0]).unwrap().primary.name,
            "workflow-wide"
        );

        def.model_chain = None;
        assert!(def.resolve_step_chain(&def.steps[0]).is_none());
    }

    #[test]
    fn workflow_definition_hydrates_cron_trigger() {
        let new = NewWorkflowDefinition::builder()
            .project_id(ProjectId::new())
            .name("scheduled-flow")
            .trigger(WorkflowTrigger::Cron {
                schedule: "0 */6 * * * *".to_string(),
                timezone: Some("UTC".to_string()),
                condition: None,
            })
            .steps(vec![sample_step()])
            .build()
            .unwrap();
        let def = WorkflowDefinition::try_from_events(new.into_events()).unwrap();
        match &def.trigger {
            WorkflowTrigger::Cron {
                schedule, timezone, ..
            } => {
                assert_eq!(schedule, "0 */6 * * * *");
                assert_eq!(timezone.as_deref(), Some("UTC"));
            }
            _ => panic!("expected Cron trigger after hydration"),
        }
    }

    /// Regression for the bats failure in PR #341 CI: after `update_content`
    /// attaches a trigger with a CEL `condition:`, hydrating fresh from the
    /// event log must reproduce the condition. Previously slipped through
    /// `try_from_events` cleanly in isolation but failed end-to-end —
    /// pinning the round-trip here narrows the search space if it breaks
    /// again.
    #[test]
    fn update_content_preserves_trigger_condition_through_hydration() {
        let mut def = build();
        let res = def.update_content(
            None,
            None,
            Some(WorkflowTrigger::Manual {
                condition: Some("trigger.payload.env == 'staging'".to_string()),
            }),
            None,
            None,
            None,
        );
        assert!(matches!(res, Idempotent::Executed(())));
        assert_eq!(
            def.trigger.condition(),
            Some("trigger.payload.env == 'staging'"),
            "in-memory trigger should reflect the update"
        );

        let events = def.events.clone();
        let hydrated = WorkflowDefinition::try_from_events(events).unwrap();
        assert_eq!(
            hydrated.trigger.condition(),
            Some("trigger.payload.env == 'staging'"),
            "hydrated trigger should carry the condition from the Updated event"
        );
    }

    /// Serialize a definition's events to JSON and back, then hydrate.
    /// Catches any event-shape regression where the new `condition` field
    /// is dropped during persistence (the actual repo serializes events
    /// via serde_json into a JSONB column).
    #[test]
    fn update_content_preserves_trigger_condition_through_json_roundtrip() {
        let mut def = build();
        let _ = def.update_content(
            None,
            None,
            Some(WorkflowTrigger::Manual {
                condition: Some("trigger.payload.env == 'staging'".to_string()),
            }),
            None,
            None,
            None,
        );
        let raw_events: Vec<serde_json::Value> = def
            .events
            .iter_all()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let updated = raw_events
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("updated"))
            .expect("updated event present");
        let trigger = updated
            .get("trigger")
            .expect("updated event carries trigger field");
        assert_eq!(trigger.get("type").and_then(|t| t.as_str()), Some("manual"));
        assert_eq!(
            trigger.get("condition").and_then(|c| c.as_str()),
            Some("trigger.payload.env == 'staging'"),
            "condition must round-trip through JSON"
        );
    }

    #[test]
    fn workflow_definition_hydrates_preexisting_sandbox_decl() {
        let new = NewWorkflowDefinition::builder()
            .project_id(ProjectId::new())
            .name("uses-existing")
            .trigger(WorkflowTrigger::Manual { condition: None })
            .steps(vec![sample_step()])
            .sandboxes(vec![WorkflowSandboxDecl::Preexisting {
                name: "investigation".to_string(),
            }])
            .build()
            .unwrap();
        let def = WorkflowDefinition::try_from_events(new.into_events()).unwrap();
        assert_eq!(def.sandboxes.len(), 1);
        assert!(matches!(
            &def.sandboxes[0],
            WorkflowSandboxDecl::Preexisting { name } if name == "investigation"
        ));
    }
}
