pub(crate) mod cron_job;
pub mod definition;
pub mod entity;
pub mod error;
pub mod executor;
pub mod importer;
pub(crate) mod job;
pub(crate) mod repo;
pub mod run;
pub mod template;
pub mod yaml;

pub use importer::WorkflowsImporter;

use std::sync::Arc;

use drua_library::{CommitAttribution, WriteOp};
use rand::RngCore;
use tracing::instrument;

use crate::agent::Agents;
use crate::audit::{Audit, InteractionOutcome};
use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::skill::file::slugify;
use crate::skill::Skills;
use crate::toolset::ToolSets;
use crate::user::Users;
use yaml::canonical_workflow_path;

pub const WORKFLOW_DOC_TYPE: drua_library::DocType = drua_library::DocType::new("workflow");

pub use definition::{
    default_output_schema, parse_cron_schedule, parse_timezone, OutputSchema, OutputSchemaError,
    WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger,
};
pub use entity::*;
pub use error::*;
pub use run::{StepResult, WorkflowRun, WorkflowRunRepo, WorkflowRunState, WorkflowStepState};

pub struct ProviderFanOutResult {
    pub triggered: usize,
    pub filtered: usize,
    pub errored: usize,
    pub resumed: usize,
}

use cron_job::{cron_queue_id, TriggerCronConfig, TriggerCronJobInitializer};
use job::{ExecuteRunConfig, ExecuteRunJobInitializer};
use repo::WorkflowDefinitionRepo;
use run::entity::NewWorkflowRun;
use template::TemplateRef;

/// Audit action recorded when a `trigger.condition:` evaluates to
/// `false` and the run is deliberately suppressed.
const AUDIT_ACTION_TRIGGER_FILTERED: &str = "workflow.trigger_filtered";
/// Audit action recorded when a `trigger.condition:` errors at
/// runtime (compile, runtime, non-boolean result).
const AUDIT_ACTION_TRIGGER_CONDITION_ERRORED: &str = "workflow.trigger_condition_errored";

/// Evaluate each output's CEL expression against the resume context.
/// Builds the CEL context once and reuses it across all expressions.
/// Failed expressions log a warning and produce `null` for that key.
fn extract_wait_output(
    ctx: &template::ResumeContext<'_>,
    outputs: &std::collections::BTreeMap<String, String>,
) -> serde_json::Value {
    let built = match ctx.build() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build resume CEL context; outputs default to null");
            return serde_json::Value::Object(
                outputs
                    .keys()
                    .map(|k| (k.clone(), serde_json::Value::Null))
                    .collect(),
            );
        }
    };
    let mut out = serde_json::Map::with_capacity(outputs.len());
    for (key, expr) in outputs {
        let value = match built.evaluate_extract(expr) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(key, expr, error = %e, "output CEL evaluation failed");
                serde_json::Value::Null
            }
        };
        out.insert(key.clone(), value);
    }
    serde_json::Value::Object(out)
}

/// SHA-256 hex digest of the JSON-encoded trigger context. Stored
/// in audit metadata so operators can correlate filtered events
/// to specific deliveries without persisting the full payload
/// (memo 019e20a2 §7).
fn payload_digest(trigger_context: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(trigger_context).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

fn record_trigger_filtered_audit(
    definition: &WorkflowDefinition,
    condition_body: &str,
    trigger_context: &serde_json::Value,
) {
    // Overwrite (not `_if_unset`): MCP trigger paths pre-set
    // `action = "workflow.trigger"` before invoking the gate; the
    // gate's outcome must replace it so the audit row records the
    // actual disposition, not the verb.
    Audit::record_action(AUDIT_ACTION_TRIGGER_FILTERED);
    Audit::record_workflow_id(definition.id);
    Audit::record_metadata(serde_json::json!({
        "definition_id": definition.id.to_string(),
        "trigger_kind": definition.trigger.kind_label(),
        "condition_body": condition_body,
        "payload_digest": payload_digest(trigger_context),
    }));
    Audit::record_outcome_if_unset(InteractionOutcome::Success);
    tracing::info!(
        definition_id = %definition.id,
        condition_body,
        "workflow run suppressed by trigger condition"
    );
}

fn record_trigger_condition_errored_audit(
    definition: &WorkflowDefinition,
    condition_body: &str,
    trigger_context: &serde_json::Value,
    reason: &str,
) {
    // See `record_trigger_filtered_audit` — same overwrite rationale.
    Audit::record_action(AUDIT_ACTION_TRIGGER_CONDITION_ERRORED);
    Audit::record_workflow_id(definition.id);
    Audit::record_metadata(serde_json::json!({
        "definition_id": definition.id.to_string(),
        "trigger_kind": definition.trigger.kind_label(),
        "condition_body": condition_body,
        "payload_digest": payload_digest(trigger_context),
        "error": reason,
    }));
    Audit::record_error(reason.to_string());
    tracing::warn!(
        definition_id = %definition.id,
        condition_body,
        error = reason,
        "trigger condition errored; run not created"
    );
}

/// CEL identifier shape — `[A-Za-z_][A-Za-z0-9_]*`. Step names
/// must conform so `${{ steps.<name>.outputs.… }}` parses as a
/// field path rather than an arithmetic expression. Mirrors the
/// regex in `template::referenced_step_names`.
fn is_cel_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reject `${{ steps.X.outputs.… }}` refs whose `X` hasn't been
/// validated yet. Trigger refs always pass (provider payload schema
/// validation deferred — memo `019e01a4` open Q6).
fn reject_forward_step_refs(
    step_name: &str,
    r: &TemplateRef,
    seen: &std::collections::HashSet<String>,
) -> Result<(), WorkflowError> {
    for target in template::referenced_step_names(r) {
        if !seen.contains(&target) {
            return Err(WorkflowError::InvalidTemplateRef(format!(
                "step '{step_name}': {} — references step '{target}' which is not declared earlier in the workflow",
                r.raw
            )));
        }
    }
    Ok(())
}

fn validate_ref_against_prior_steps(
    step_name: &str,
    r: &TemplateRef,
    seen: &std::collections::HashSet<String>,
) -> Result<(), WorkflowError> {
    template::validate_root(r)
        .map_err(|e| WorkflowError::InvalidTemplateRef(format!("step '{step_name}': {e}")))?;
    reject_forward_step_refs(step_name, r, seen)
}

#[derive(Clone)]
pub struct Workflows {
    repo: WorkflowDefinitionRepo,
    run_repo: WorkflowRunRepo,
    skills: Arc<Skills>,
    users: Arc<Users>,
    /// Held for parse-time `${{ … }}` reference validation in
    /// `tool_step.params` — looks up the called tool's
    /// `output_schema()` + `composable()` flag.
    toolsets: Arc<ToolSets>,
    library: drua_library::Library,
    agents: Arc<Agents>,
    sandboxes: Arc<Sandboxes>,
    execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
    cron_spawner: ::job::JobSpawner<TriggerCronConfig>,
}

impl Workflows {
    /// Wires the workflow service: registers the `ExecuteRun` and
    /// `TriggerCron` job initializers with `jobs`. Cron schedules
    /// persist as job rows, so no startup-recovery is needed — the
    /// poller will pick them up on its first tick.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        pool: &sqlx::PgPool,
        library: drua_library::Library,
        skills: Arc<Skills>,
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        users: Arc<Users>,
        toolsets: Arc<ToolSets>,
        jobs: &mut ::job::Jobs,
    ) -> Self {
        let execute_run_spawner = jobs.add_initializer(ExecuteRunJobInitializer::new(
            WorkflowRunRepo::new(pool),
            WorkflowDefinitionRepo::new_without_library(pool),
            Arc::clone(&agents),
            Arc::clone(&skills),
            Arc::clone(&sandboxes),
            Arc::clone(&toolsets),
        ));
        let cron_spawner = jobs.add_initializer(TriggerCronJobInitializer::new(
            WorkflowDefinitionRepo::new_without_library(pool),
            WorkflowRunRepo::new(pool),
            execute_run_spawner.clone(),
        ));
        Self {
            repo: WorkflowDefinitionRepo::new(pool, library.clone()),
            run_repo: WorkflowRunRepo::new(pool),
            skills,
            users,
            toolsets,
            library,
            agents,
            sandboxes,
            execute_run_spawner,
            cron_spawner,
        }
    }

    /// Reverse-sync entry point: persist a `ParsedWorkflow` produced
    /// by the library importer. Creates or updates depending on
    /// whether the workflow already exists. `Ok(None)` signals
    /// idempotency — the existing entity's `file_hash` matches the
    /// incoming bytes, so the caller should skip search re-upsert +
    /// embed re-spawn.
    pub(crate) async fn import_from_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        parsed: yaml::ParsedWorkflow,
        project_id: ProjectId,
    ) -> Result<Option<WorkflowDefinition>, WorkflowError> {
        let yaml::ParsedWorkflow {
            workflow_id,
            project_name,
            name,
            description,
            trigger,
            steps,
            sandboxes,
            model_chain,
            original_path,
            rendered,
            ..
        } = parsed;

        Self::validate_trigger(&trigger)?;

        if self.repo.is_soft_deleted_in_op(op, workflow_id).await? {
            tracing::info!(
                %workflow_id,
                "skipping reverse-sync import for soft-deleted workflow"
            );
            return Ok(None);
        }

        let file_hash = drua_library::GitFileHash::new(rendered);

        if let Some(mut existing) = self.repo.maybe_find_by_id(workflow_id).await? {
            // Same non-cron → cron rule as `update`: only a fresh
            // transition needs a spawn; cron → cron is picked up by
            // the in-flight job, cron → other is handled by the
            // runner's terminate-on-not-cron branch.
            let was_cron = matches!(existing.trigger, WorkflowTrigger::Cron { .. });
            if !existing
                .update_from_library(
                    Some(name.clone()),
                    Some(description.clone()),
                    Some(trigger),
                    Some(steps),
                    Some(sandboxes),
                    Some(model_chain.clone()),
                    file_hash,
                )
                .did_execute()
            {
                return Ok(None);
            }
            self.repo.update_in_op(op, &mut existing).await?;
            let is_cron = matches!(existing.trigger, WorkflowTrigger::Cron { .. });
            if !was_cron && is_cron {
                self.register_cron_in_op(op, &existing).await?;
            }
            return Ok(Some(existing));
        }

        let trigger = match trigger {
            WorkflowTrigger::Webhook {
                provider,
                secret,
                condition,
            } if secret.is_empty() => WorkflowTrigger::Webhook {
                provider,
                secret: Self::generate_webhook_secret(),
                condition,
            },
            other => other,
        };

        let mut builder = NewWorkflowDefinition::builder()
            .id(workflow_id)
            .project_id(project_id)
            .name(name)
            .trigger(trigger)
            .steps(steps)
            .sandboxes(sandboxes)
            .model_chain(model_chain);
        if let Some(project) = project_name {
            builder = builder.project_name(project);
        }
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        builder = builder.original_path(original_path);
        let new = builder
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;
        let created = self.repo.create_in_op(op, new).await?;
        self.register_cron_in_op(op, &created).await?;
        Ok(Some(created))
    }

    fn generate_webhook_secret() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        format!("whsec_{}", hex::encode(bytes))
    }

    /// Reject cron triggers whose schedule or timezone won't parse so
    /// the failure surfaces at create-time, not on the first fire.
    fn validate_trigger(trigger: &WorkflowTrigger) -> Result<(), WorkflowError> {
        if let WorkflowTrigger::Cron {
            schedule, timezone, ..
        } = trigger
        {
            parse_cron_schedule(schedule).map_err(WorkflowError::InvalidCronExpression)?;
            parse_timezone(timezone.as_deref()).map_err(WorkflowError::InvalidTimezone)?;
        }
        if let Some(body) = trigger.condition() {
            template::parse_trigger_condition(body)
                .map_err(|e| WorkflowError::InvalidCondition(format!("trigger: {e}")))?;
        }
        Ok(())
    }

    /// Resolve `skill:` references and validate sandbox references
    /// against the workflow's own declarations. Project skills win
    /// over sandbox-exported skills; an agent step backed by a
    /// `Preexisting` sandbox may also resolve its skill from that
    /// sandbox's exported set.
    async fn validate_steps(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        steps: &[WorkflowStepDef],
        sandboxes: &[WorkflowSandboxDecl],
    ) -> Result<(), WorkflowError> {
        let mut decl_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for decl in sandboxes {
            if !decl_names.insert(decl.name()) {
                return Err(WorkflowError::DuplicateSandboxName(decl.name().to_string()));
            }
        }

        // Resolve every Preexisting decl up-front: look up the named
        // sandbox in the project + auth-check Read. The map is
        // consulted per-step so a sandbox-only skill counts as
        // resolved when its step targets a Preexisting sandbox.
        let preexisting_ids = self
            .resolve_preexisting_sandboxes(sub, project_id, sandboxes)
            .await?;

        // Collect every bad reference in a single pass so the operator
        // sees the full list (rather than fixing one and re-running).
        let mut missing_skills: Vec<(String, String)> = Vec::new();
        let mut undeclared_sandboxes: Vec<(String, String)> = Vec::new();
        // Names of steps that have already been validated, in order;
        // a step's `${{ steps.X.outputs.… }}` refs may only point at
        // entries already in this set (i.e. earlier in the linear
        // step list).
        let mut seen_step_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Step names appear inside `${{ steps.<name>.outputs.… }}`
        // and must therefore be valid CEL identifiers — anything
        // else (hyphens, spaces, leading digits, …) is parsed by
        // CEL as a different expression entirely (e.g.
        // `steps.store-note.outputs.x` becomes
        // `(steps.store) - (note.outputs.x)`). Reject at parse time
        // so authors learn at create time, not when a downstream
        // template silently fails to resolve.
        for step in steps {
            if !is_cel_identifier(step.name()) {
                return Err(WorkflowError::InvalidStep(format!(
                    "step '{}': name must be a CEL identifier (letters, digits, \
                     underscores; first char a letter or underscore) so it can be \
                     referenced via `${{{{ steps.<name>.outputs.… }}}}`",
                    step.name()
                )));
            }
        }

        for step in steps {
            // Compile + root-validate + forward-ref-check the
            // optional `condition:` body. Lives on both step kinds,
            // so factor the check out of the per-variant arms.
            if let Some(body) = step.condition() {
                let r = template::parse_condition(body).map_err(|e| {
                    WorkflowError::InvalidCondition(format!("step '{}': {e}", step.name()))
                })?;
                validate_ref_against_prior_steps(step.name(), &r, &seen_step_names)?;
            }

            match step {
                WorkflowStepDef::AgentStep {
                    name,
                    skill,
                    sandbox,
                    ..
                } => {
                    let preexisting_sandbox_id = sandbox
                        .as_deref()
                        .and_then(|n| preexisting_ids.get(n).copied());
                    // `find_by_name` resolves the project's mounted-space
                    // skills internally via the held `SpaceMounts`, so a
                    // workflow step referring to a space-scoped skill by
                    // name is reachable as long as the workflow's project
                    // mounts the space.
                    let found = self
                        .skills
                        .find_by_name(skill, Some(project_id), preexisting_sandbox_id)
                        .await
                        .map_err(|e| WorkflowError::Skill(e.to_string()))?;
                    if found.is_none() {
                        missing_skills.push((name.clone(), skill.clone()));
                    }

                    if let Some(sandbox_name) = sandbox {
                        if !decl_names.contains(sandbox_name.as_str()) {
                            undeclared_sandboxes.push((name.clone(), sandbox_name.clone()));
                        }
                    }

                    // `OutputSchema` enforces the `type: "object"` root
                    // invariant at construction/deserialization
                    // (memo 019dfc8c). Bad schemas can't reach here.
                }
                WorkflowStepDef::Wait {
                    name,
                    provider,
                    resume_condition,
                    outputs,
                    ..
                } => {
                    if provider.is_empty() {
                        return Err(WorkflowError::InvalidStep(format!(
                            "wait step '{name}': `provider` must be a non-empty string"
                        )));
                    }
                    let r =
                        template::parse_resume_condition(resume_condition.trim()).map_err(|e| {
                            WorkflowError::InvalidCondition(format!(
                                "wait step '{name}': resume_condition: {e}"
                            ))
                        })?;
                    reject_forward_step_refs(name, &r, &seen_step_names)?;
                    for (key, expr) in outputs {
                        let r = template::parse_resume_condition(expr.trim()).map_err(|e| {
                            WorkflowError::InvalidTemplateRef(format!(
                                "wait step '{name}': outputs.{key}: {e}"
                            ))
                        })?;
                        reject_forward_step_refs(name, &r, &seen_step_names)?;
                    }
                }
                WorkflowStepDef::ToolStep {
                    name, tool, params, ..
                } => {
                    // The lookup enforces every workflow `tool_step`
                    // contract in one place: registered, composable,
                    // declares an output_schema. The runtime
                    // executor calls the same helper, so a workflow
                    // that passes `validate_steps` cannot fail
                    // dispatch on these checks at run time.
                    self.toolsets.find_for_workflow(tool).map_err(|e| {
                        WorkflowError::InvalidStep(format!("tool_step '{name}': {e}"))
                    })?;

                    // `validate_ref_against_prior_steps` runs
                    // `template::validate_root` internally — no need
                    // to repeat it here.
                    let refs = template::extract_refs_in_value(params).map_err(|e| {
                        WorkflowError::InvalidTemplateRef(format!("tool_step '{name}': {e}"))
                    })?;
                    for r in &refs {
                        validate_ref_against_prior_steps(name, r, &seen_step_names)?;
                    }
                }
            }
            seen_step_names.insert(step.name().to_string());
        }

        if !missing_skills.is_empty() {
            let listed = missing_skills
                .iter()
                .map(|(step, skill)| format!("step '{step}' → skill '{skill}'"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(WorkflowError::SkillNotFound(format!(
                "{listed}. The `skill` field expects the NAME of a skill in this project; create skills first via the `skill` tool"
            )));
        }
        if !undeclared_sandboxes.is_empty() {
            let listed = undeclared_sandboxes
                .iter()
                .map(|(step, sb)| format!("step '{step}' → sandbox '{sb}'"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(WorkflowError::UndeclaredSandbox(format!(
                "{listed}. Add each name to the workflow's top-level `sandboxes` declarations"
            )));
        }
        Ok(())
    }

    /// For every `Preexisting` decl, look the sandbox up by name in
    /// the workflow's project (project-unique) and verify `sub`
    /// can `Read` it. Returns a map from decl name to resolved
    /// `SandboxId` so the skill lookup can fall back to its exports.
    async fn resolve_preexisting_sandboxes(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        sandboxes: &[WorkflowSandboxDecl],
    ) -> Result<std::collections::HashMap<String, SandboxId>, WorkflowError> {
        let mut out = std::collections::HashMap::new();
        for decl in sandboxes {
            let WorkflowSandboxDecl::Preexisting { name } = decl else {
                continue;
            };
            let sb = self
                .skills
                .sandboxes()
                .find_by_name_in_project(sub, project_id, name)
                .await
                .map_err(|e| {
                    WorkflowError::SandboxNotFound(format!("preexisting sandbox '{name}': {e}"))
                })?;
            out.insert(name.clone(), sb.id);
        }
        Ok(out)
    }

    /// `project_name` is cached on the entity so the forward-sync
    /// hook can render the library path without an extra lookup.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "core.workflow.create", skip_all)]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        project_name: &str,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
        sandboxes: Vec<WorkflowSandboxDecl>,
        model_chain: Option<llm::ModelChain>,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        sub.can(AuthVerb::Create, AuthResource::Workflow(project_id, None))?;

        if steps.is_empty() {
            return Err(WorkflowError::InvalidDefinition(
                "workflow requires at least one step".into(),
            ));
        }

        Self::validate_trigger(&trigger)?;

        self.validate_steps(sub, project_id, &steps, &sandboxes)
            .await?;

        let trigger = match trigger {
            WorkflowTrigger::Webhook {
                provider,
                secret,
                condition,
            } if secret.is_empty() => WorkflowTrigger::Webhook {
                provider,
                secret: Self::generate_webhook_secret(),
                condition,
            },
            other => other,
        };

        let mut builder = NewWorkflowDefinition::builder()
            .project_id(project_id)
            .name(name)
            .trigger(trigger)
            .steps(steps)
            .sandboxes(sandboxes)
            .model_chain(model_chain);
        if !project_name.is_empty() {
            builder = builder.project_name(project_name);
        }
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new = builder
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let mut op = self.repo.begin_op().await?;
        self.populate_attribution().await;
        let workflow = self.repo.create_in_op(&mut op, new).await?;
        self.register_cron_in_op(&mut op, &workflow).await?;
        op.commit().await?;
        Ok(workflow)
    }

    /// `Some` applies the field; `None` leaves it untouched.
    /// `model_chain: Some(None)` clears the field.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "core.workflow.update", skip_all)]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        id: WorkflowDefinitionId,
        name: Option<String>,
        description: Option<Option<String>>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        sandboxes: Option<Vec<WorkflowSandboxDecl>>,
        model_chain: Option<Option<llm::ModelChain>>,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        let mut definition = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::Workflow(definition.project_id, Some(definition.id)),
        )?;

        if let Some(t) = trigger.as_ref() {
            Self::validate_trigger(t)?;
        }

        let next_steps = steps.as_ref().unwrap_or(&definition.steps);
        let next_sandboxes = sandboxes.as_ref().unwrap_or(&definition.sandboxes);
        if next_steps.is_empty() {
            return Err(WorkflowError::InvalidDefinition(
                "workflow requires at least one step".into(),
            ));
        }
        if steps.is_some() || sandboxes.is_some() {
            self.validate_steps(sub, definition.project_id, next_steps, next_sandboxes)
                .await?;
        }

        // Capture before `update_content` mutates `definition.trigger`.
        // Only a non-cron → cron transition needs a fresh spawn. A
        // cron → cron schedule change is picked up by the in-flight
        // job's next fire (it re-reads the definition); cron → other
        // is handled by the runner exiting when the trigger is no
        // longer `Cron`. Re-spawning in either of those cases
        // duplicates the chain.
        let was_cron = matches!(definition.trigger, WorkflowTrigger::Cron { .. });
        if definition
            .update_content(name, description, trigger, steps, sandboxes, model_chain)
            .did_execute()
        {
            let mut op = self.repo.begin_op().await?;
            self.populate_attribution().await;
            self.repo.update_in_op(&mut op, &mut definition).await?;
            let is_cron = matches!(definition.trigger, WorkflowTrigger::Cron { .. });
            if !was_cron && is_cron {
                self.register_cron_in_op(&mut op, &definition).await?;
            }
            op.commit().await?;
        }
        Ok(definition)
    }

    async fn populate_attribution(&self) -> CommitAttribution {
        self.users.commit_attribution().await
    }

    async fn enqueue_workflow_delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workflow: &WorkflowDefinition,
        attribution: CommitAttribution,
    ) -> Result<(), WorkflowError> {
        let path = canonical_workflow_path(&workflow.name, workflow.project_name.as_deref());
        let id_uuid: uuid::Uuid = workflow.id.into();
        let message = format!(
            "workflow: delete {}-{}",
            slugify(&workflow.name),
            &id_uuid.to_string()[..8]
        );
        self.library
            .enqueue_full_in_op(
                op,
                None,
                vec![(id_uuid, WORKFLOW_DOC_TYPE)],
                Some(WriteOp::DeleteFile { path, message }),
                attribution,
            )
            .await?;
        Ok(())
    }

    /// Spawn the next cron fire in the same atomic op as the
    /// workflow create/update. Either both land or both roll back, so
    /// no startup-recovery sweep is needed: the schedule is durably
    /// queued whenever its definition exists. No-op for non-cron
    /// triggers.
    async fn register_cron_in_op<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        workflow: &WorkflowDefinition,
    ) -> Result<(), WorkflowError> {
        let WorkflowTrigger::Cron { schedule, .. } = &workflow.trigger else {
            return Ok(());
        };
        let next_at = match workflow
            .trigger
            .next_fire_at(chrono::Utc::now())
            .map_err(WorkflowError::InvalidCronExpression)?
        {
            Some(t) => t,
            None => {
                tracing::warn!(
                    workflow_id = %workflow.id,
                    %schedule,
                    "cron expression has no future fires; skipping registration"
                );
                return Ok(());
            }
        };
        self.cron_spawner
            .spawn_at_with_queue_id_in_op(
                op,
                ::job::JobId::new(),
                TriggerCronConfig {
                    definition_id: workflow.id,
                },
                next_at,
                cron_queue_id(workflow.id),
            )
            .await
            .map_err(|e| WorkflowError::Job(e.to_string()))?;
        Ok(())
    }

    #[instrument(name = "core.workflow.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: WorkflowDefinitionId,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        let workflow = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(workflow.project_id, Some(workflow.id)),
        )?;
        Ok(workflow)
    }

    /// Bypasses auth — internal callers (the webhook handler) verify
    /// via the trigger's stored secret instead.
    #[instrument(name = "core.workflow.find_by_id_unchecked", skip_all)]
    pub async fn find_by_id_unchecked(
        &self,
        id: WorkflowDefinitionId,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "core.workflow.list_for_project", skip_all)]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<WorkflowDefinition>, WorkflowError> {
        sub.can(AuthVerb::Read, AuthResource::Workflow(project_id, None))?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    /// Spawns the executor as a job and returns the run synchronously.
    /// `Ok(None)` means the trigger's `condition:` evaluated to false
    /// and the run was deliberately suppressed (an audit row was
    /// written).
    #[instrument(name = "core.workflow.trigger_run", skip_all)]
    pub async fn trigger_run(
        &self,
        sub: &AuthSubject,
        definition_id: WorkflowDefinitionId,
        trigger_context: serde_json::Value,
    ) -> Result<Option<WorkflowRun>, WorkflowError> {
        let definition = self.repo.find_by_id(definition_id).await?;
        sub.can(
            AuthVerb::Use,
            AuthResource::Workflow(definition.project_id, Some(definition.id)),
        )?;
        self.spawn_run(definition, trigger_context).await
    }

    /// No auth check — pairs with [`Self::find_by_id_unchecked`] so
    /// the webhook handler doesn't double-load the definition.
    /// `Ok(None)` carries the same "filtered by trigger condition"
    /// meaning as [`Self::trigger_run`].
    #[instrument(name = "core.workflow.trigger_run_for_definition", skip_all)]
    pub async fn trigger_run_for_definition(
        &self,
        definition: WorkflowDefinition,
        trigger_context: serde_json::Value,
    ) -> Result<Option<WorkflowRun>, WorkflowError> {
        self.spawn_run(definition, trigger_context).await
    }

    #[instrument(name = "core.workflow.trigger_all_for_provider", skip_all)]
    pub async fn trigger_all_for_provider(
        &self,
        provider: &str,
        trigger_context: serde_json::Value,
    ) -> Result<ProviderFanOutResult, WorkflowError> {
        let mut triggered = 0usize;
        let mut filtered = 0usize;
        let mut errored = 0usize;

        let mut query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        loop {
            let mut result = self
                .repo
                .list_for_provider_by_created_at(
                    Some(provider.to_string()),
                    query,
                    es_entity::ListDirection::Descending,
                )
                .await?;

            for defn in result.entities.drain(..) {
                let def_id = defn.id;
                match self
                    .trigger_run_for_definition(defn, trigger_context.clone())
                    .await
                {
                    Ok(Some(run)) => {
                        tracing::info!(%def_id, run_id = %run.id, "provider fan-out: run triggered");
                        triggered += 1;
                    }
                    Ok(None) => {
                        tracing::debug!(%def_id, "provider fan-out: filtered by condition");
                        filtered += 1;
                    }
                    Err(e) => {
                        tracing::warn!(%def_id, error = %e, "provider fan-out: trigger errored");
                        errored += 1;
                    }
                }
            }

            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }

        Ok(ProviderFanOutResult {
            triggered,
            filtered,
            errored,
            resumed: 0,
        })
    }

    /// Try to resume parked runs whose wait step matches the inbound
    /// event. Returns the number of runs resumed.
    #[instrument(name = "core.workflow.try_resume_by_provider", skip_all)]
    pub async fn try_resume_by_provider(
        &self,
        provider: &str,
        event: &serde_json::Value,
    ) -> Result<usize, WorkflowError> {
        let mut resumed = 0usize;
        let mut query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        loop {
            let mut result = self
                .run_repo
                .list_for_state_by_created_at(
                    WorkflowRunState::WaitingForEvent,
                    query,
                    es_entity::ListDirection::Descending,
                )
                .await?;

            for mut run in result.entities.drain(..) {
                let wait_step_name = match run.current_wait_step() {
                    Some(sr) => sr.name.clone(),
                    None => continue,
                };

                let wait_def = run
                    .steps_snapshot
                    .iter()
                    .find(|s| s.name() == wait_step_name);

                let (step_provider, resume_condition, outputs) = match wait_def {
                    Some(WorkflowStepDef::Wait {
                        provider: p,
                        resume_condition,
                        outputs,
                        ..
                    }) => (p.as_str(), resume_condition.as_str(), outputs),
                    _ => continue,
                };

                if !step_provider.eq_ignore_ascii_case(provider) {
                    continue;
                }

                let step_outputs: std::collections::HashMap<String, serde_json::Value> = run
                    .step_results
                    .iter()
                    .map(|r| {
                        let val = r.output.clone().unwrap_or(serde_json::Value::Null);
                        (r.name.clone(), val)
                    })
                    .collect();

                let ctx = template::ResumeContext {
                    trigger: &run.trigger_context,
                    steps: &step_outputs,
                    resume_payload: event,
                };

                match ctx.evaluate_condition(resume_condition) {
                    Ok(template::ConditionOutcome::True) => {}
                    Ok(outcome) => {
                        tracing::debug!(run_id = %run.id, ?outcome, "resume_condition not satisfied");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(run_id = %run.id, err = %e, "resume_condition evaluation failed");
                        continue;
                    }
                }

                let extracted = extract_wait_output(&ctx, outputs);

                let source = format!("provider:{provider}");
                if run
                    .step_resumed(wait_step_name, extracted, source)
                    .did_execute()
                {
                    match self.commit_resume(&mut run).await {
                        Ok(()) => {
                            tracing::info!(run_id = %run.id, "resumed parked run");
                            resumed += 1;
                        }
                        Err(e) => {
                            tracing::warn!(run_id = %run.id, err = %e, "failed to commit resume");
                        }
                    }
                }
            }

            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }
        Ok(resumed)
    }

    async fn commit_resume(&self, run: &mut WorkflowRun) -> Result<(), WorkflowError> {
        let mut op = self.run_repo.begin_op().await?;
        self.run_repo.update_in_op(&mut op, run).await?;
        let queue_id = format!("workflow:{}", run.definition_id);
        // Re-spawn under a fresh JobId: the run's original ExecuteRun job (id =
        // run.id) still occupies the `jobs` row, so reusing it would hit the
        // primary-key constraint and roll the whole resume back. Parenting on
        // run.id keeps the lineage queryable via list_by_parent_job_id.
        self.execute_run_spawner
            .clone()
            .with_parent(run.id.into())
            .spawn_with_queue_id_in_op(
                &mut op,
                ::job::JobId::new(),
                ExecuteRunConfig { run_id: run.id },
                &queue_id,
            )
            .await
            .map_err(|e| WorkflowError::Job(e.to_string()))?;
        op.commit().await?;
        Ok(())
    }

    async fn spawn_run(
        &self,
        definition: WorkflowDefinition,
        trigger_context: serde_json::Value,
    ) -> Result<Option<WorkflowRun>, WorkflowError> {
        Self::spawn_run_static(
            &self.run_repo,
            &self.execute_run_spawner,
            definition,
            trigger_context,
        )
        .await
    }

    pub(crate) async fn spawn_run_static(
        run_repo: &WorkflowRunRepo,
        execute_run_spawner: &::job::JobSpawner<ExecuteRunConfig>,
        definition: WorkflowDefinition,
        trigger_context: serde_json::Value,
    ) -> Result<Option<WorkflowRun>, WorkflowError> {
        if let Some(body) = definition.trigger.condition() {
            match template::evaluate_trigger_condition(body, &trigger_context) {
                Ok(template::ConditionOutcome::True) => {}
                Ok(template::ConditionOutcome::False) => {
                    record_trigger_filtered_audit(&definition, body, &trigger_context);
                    return Ok(None);
                }
                Ok(template::ConditionOutcome::NotBoolean(rendered)) => {
                    let reason = format!("condition evaluated to non-boolean: {rendered}");
                    record_trigger_condition_errored_audit(
                        &definition,
                        body,
                        &trigger_context,
                        &reason,
                    );
                    return Err(WorkflowError::TriggerConditionErrored {
                        body: body.to_string(),
                        reason,
                    });
                }
                Err(e) => {
                    let reason = e.to_string();
                    record_trigger_condition_errored_audit(
                        &definition,
                        body,
                        &trigger_context,
                        &reason,
                    );
                    return Err(WorkflowError::TriggerConditionErrored {
                        body: body.to_string(),
                        reason,
                    });
                }
            }
        }

        let new = NewWorkflowRun::builder()
            .definition_id(definition.id)
            .project_id(definition.project_id)
            .trigger_context(trigger_context)
            .steps_snapshot(definition.steps.clone())
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let run = run_repo.create(new).await?;

        let queue_id = format!("workflow:{}", definition.id);
        execute_run_spawner
            .spawn_with_queue_id(run.id, ExecuteRunConfig { run_id: run.id }, &queue_id)
            .await
            .map_err(|e| WorkflowError::Job(e.to_string()))?;

        Ok(Some(run))
    }

    #[instrument(name = "core.workflow.list_runs", skip_all)]
    pub async fn list_runs(
        &self,
        sub: &AuthSubject,
        definition_id: WorkflowDefinitionId,
    ) -> Result<Vec<WorkflowRun>, WorkflowError> {
        let definition = self.repo.find_by_id(definition_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(definition.project_id, Some(definition.id)),
        )?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .run_repo
            .list_for_definition_id_by_created_at(
                definition_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "core.workflow.find_run_by_id", skip_all)]
    pub async fn find_run_by_id(
        &self,
        sub: &AuthSubject,
        run_id: WorkflowRunId,
    ) -> Result<WorkflowRun, WorkflowError> {
        let run = self.run_repo.find_by_id(run_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(run.project_id, Some(run.definition_id)),
        )?;
        Ok(run)
    }

    #[instrument(name = "core.workflow.delete_for_project_in_op", skip_all)]
    pub(crate) async fn delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), WorkflowError> {
        // Runs FK-reference definitions, so delete runs first.
        self.run_repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        self.repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        Ok(())
    }

    #[instrument(name = "core.workflow.delete", skip_all)]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: WorkflowDefinitionId,
    ) -> Result<(), WorkflowError> {
        let workflow = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Delete,
            AuthResource::Workflow(workflow.project_id, Some(workflow.id)),
        )?;
        Audit::record_action_if_unset("workflow.delete");
        Audit::record_project_id(workflow.project_id);
        Audit::record_workflow_id(workflow.id);

        let mut op = self.repo.begin_op().await?;
        let attribution = self.populate_attribution().await;
        self.enqueue_workflow_delete_in_op(&mut op, &workflow, attribution)
            .await?;
        self.agents
            .cascade_delete_for_workflow_id_in_op(&mut op, id)
            .await?;
        self.sandboxes
            .cascade_delete_for_workflow_id_in_op(&mut op, id)
            .await?;
        self.run_repo
            .cascade_delete_for_definition_in_op(&mut op, id)
            .await?;
        self.repo.delete_in_op(&mut op, workflow).await?;
        op.commit().await?;
        Ok(())
    }

    pub(crate) async fn delete_by_path_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
        path: &str,
    ) -> Result<Option<WorkflowDefinitionId>, WorkflowError> {
        let Some(workflow) = self
            .repo
            .maybe_find_by_canonical_path_in_op(op, project_id, path)
            .await?
        else {
            return Ok(None);
        };
        let id = workflow.id;
        self.library
            .search()
            .delete_in_op(op, id.into(), WORKFLOW_DOC_TYPE)
            .await?;
        self.agents
            .cascade_delete_for_workflow_id_in_op(op, id)
            .await?;
        self.sandboxes
            .cascade_delete_for_workflow_id_in_op(op, id)
            .await?;
        self.run_repo
            .cascade_delete_for_definition_in_op(op, id)
            .await?;
        self.repo.delete_in_op(op, workflow).await?;
        Ok(Some(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trigger_accepts_manual_and_webhook() {
        Workflows::validate_trigger(&WorkflowTrigger::Manual { condition: None }).unwrap();
        Workflows::validate_trigger(&WorkflowTrigger::Webhook {
            provider: Some("honeycomb".into()),
            secret: "whsec".into(),
            condition: None,
        })
        .unwrap();
    }

    #[test]
    fn validate_trigger_accepts_valid_cron() {
        Workflows::validate_trigger(&WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".into(),
            timezone: Some("UTC".into()),
            condition: None,
        })
        .unwrap();
    }

    #[test]
    fn validate_trigger_rejects_bad_cron_expression() {
        let err = Workflows::validate_trigger(&WorkflowTrigger::Cron {
            schedule: "not a cron".into(),
            timezone: None,
            condition: None,
        })
        .unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidCronExpression(_)));
    }

    #[test]
    fn validate_trigger_rejects_bad_timezone() {
        let err = Workflows::validate_trigger(&WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".into(),
            timezone: Some("Mars/Olympus_Mons".into()),
            condition: None,
        })
        .unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidTimezone(_)));
    }

    #[test]
    fn validate_trigger_rejects_malformed_cel_condition() {
        let err = Workflows::validate_trigger(&WorkflowTrigger::Webhook {
            provider: None,
            secret: "whsec".into(),
            condition: Some("trigger.payload.x ==".into()),
        })
        .unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidCondition(_)));
    }

    #[test]
    fn validate_trigger_rejects_condition_referencing_steps() {
        let err = Workflows::validate_trigger(&WorkflowTrigger::Webhook {
            provider: None,
            secret: "whsec".into(),
            condition: Some("steps.x.outputs.y == 1".into()),
        })
        .unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidCondition(_)));
    }

    #[test]
    fn validate_trigger_accepts_well_formed_condition() {
        Workflows::validate_trigger(&WorkflowTrigger::Webhook {
            provider: Some("github_app".into()),
            secret: "whsec".into(),
            condition: Some("trigger.payload.action == 'opened'".into()),
        })
        .unwrap();
    }

    #[test]
    fn template_ref_against_prior_steps_accepts_trigger() {
        let r = template::parse_path("trigger.payload.build").unwrap();
        let seen = std::collections::HashSet::new();
        validate_ref_against_prior_steps("any", &r, &seen).expect("trigger refs always accepted");
    }

    #[test]
    fn template_ref_against_prior_steps_accepts_seen_step() {
        let r = template::parse_path("steps.triage.outputs.workflow").unwrap();
        let mut seen = std::collections::HashSet::new();
        seen.insert("triage".to_string());
        validate_ref_against_prior_steps("dispatch", &r, &seen).unwrap();
    }

    #[test]
    fn template_ref_against_prior_steps_rejects_forward_ref() {
        let r = template::parse_path("steps.later.outputs.x").unwrap();
        let seen = std::collections::HashSet::new();
        let err = validate_ref_against_prior_steps("first", &r, &seen).unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidTemplateRef(_)));
    }

    #[test]
    fn template_ref_against_prior_steps_rejects_unknown_root() {
        let r = template::parse_path("env.HOME").unwrap();
        let seen = std::collections::HashSet::new();
        let err = validate_ref_against_prior_steps("any", &r, &seen).unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidTemplateRef(_)));
    }

    #[test]
    fn cel_identifier_accepts_underscore_and_alphanumerics() {
        assert!(is_cel_identifier("step1"));
        assert!(is_cel_identifier("_leading_underscore"));
        assert!(is_cel_identifier("Camel_Snake_42"));
    }

    #[test]
    fn cel_identifier_rejects_hyphen_leading_digit_and_empty() {
        assert!(!is_cel_identifier(""));
        assert!(!is_cel_identifier("store-note"));
        assert!(!is_cel_identifier("1step"));
        assert!(!is_cel_identifier("has space"));
        assert!(!is_cel_identifier("dot.in.middle"));
    }
}
