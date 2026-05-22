use std::collections::HashSet;

use serde::Serialize;

use super::definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};
use super::template;

/// CEL identifier shape: `[A-Za-z_][A-Za-z0-9_]*`.
///
/// Step names must match this so `steps.<name>.outputs...` parses as
/// a field path instead of another CEL expression.
pub(crate) fn is_cel_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkflowValidationError {
    pub(crate) code: &'static str,
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Default)]
pub(crate) struct WorkflowValidationErrors {
    pub(crate) items: Vec<WorkflowValidationError>,
}

impl WorkflowValidationErrors {
    fn error(&mut self, code: &'static str, path: impl Into<String>, message: impl Into<String>) {
        self.items.push(WorkflowValidationError {
            code,
            path: path.into(),
            message: message.into(),
        });
    }
}

#[derive(Default)]
pub(crate) struct WorkflowDefinitionValidator {
    errors: WorkflowValidationErrors,
    declared_sandboxes: HashSet<String>,
    seen_step_names: HashSet<String>,
}

impl WorkflowDefinitionValidator {
    pub(crate) fn validate_trigger(&mut self, trigger: &WorkflowTrigger) {
        if let WorkflowTrigger::Cron {
            schedule, timezone, ..
        } = trigger
        {
            if let Err(e) = super::parse_cron_schedule(schedule) {
                self.errors
                    .error("trigger.cron_invalid", "/trigger/schedule", e);
            }
            if let Err(e) = super::parse_timezone(timezone.as_deref()) {
                self.errors
                    .error("trigger.timezone_invalid", "/trigger/timezone", e);
            }
        }

        let Some(condition) = trigger.condition() else {
            return;
        };
        if let Err(e) = template::parse_trigger_condition(condition) {
            self.errors.error(
                "trigger.condition_invalid",
                "/trigger/condition",
                e.to_string(),
            );
        }
    }

    pub(crate) fn validate_sandbox_decl(&mut self, index: usize, decl: &WorkflowSandboxDecl) {
        if !self.declared_sandboxes.insert(decl.name().to_string()) {
            self.errors.error(
                "sandbox.duplicate_name",
                format!("/sandboxes/{index}/name"),
                format!("sandbox name `{}` is declared more than once", decl.name()),
            );
        }
    }

    pub(crate) fn record_preexisting_sandbox_missing(
        &mut self,
        index: usize,
        name: &str,
        error: impl std::fmt::Display,
    ) {
        self.errors.error(
            "sandbox.preexisting_missing",
            format!("/sandboxes/{index}/name"),
            format!("preexisting sandbox `{name}` could not be resolved: {error}"),
        );
    }

    pub(crate) fn validate_steps_present(&mut self, steps: &[WorkflowStepDef]) {
        if steps.is_empty() {
            self.errors.error(
                "workflow.steps_empty",
                "/steps",
                "workflow requires at least one step",
            );
        }
    }

    pub(crate) fn validate_step_static(&mut self, index: usize, step: &WorkflowStepDef) {
        let step_path = format!("/steps/{index}");
        if !is_cel_identifier(step.name()) {
            self.errors.error(
                "step.name_not_cel_identifier",
                format!("{step_path}/name"),
                "step name must be a CEL identifier so downstream `steps.<name>.outputs` references can resolve",
            );
        }
        if let Some(body) = step.condition() {
            match template::parse_condition(body) {
                Ok(r) => self.record_forward_step_refs(
                    &r,
                    "step.condition_forward_ref",
                    &format!("{step_path}/condition"),
                    "condition",
                ),
                Err(e) => self.errors.error(
                    "step.condition_invalid",
                    format!("{step_path}/condition"),
                    e.to_string(),
                ),
            }
        }

        match step {
            WorkflowStepDef::AgentStep { name, sandbox, .. } => {
                if let Some(sandbox_name) = sandbox {
                    if !self.declared_sandboxes.contains(sandbox_name) {
                        self.errors.error(
                            "step.sandbox_undeclared",
                            format!("{step_path}/sandbox"),
                            format!(
                                "step `{name}` references sandbox `{sandbox_name}` that is not declared"
                            ),
                        );
                    }
                }
            }
            WorkflowStepDef::ToolStep { name, params, .. } => {
                match template::extract_refs_in_value(params) {
                    Ok(refs) => {
                        for r in refs {
                            if let Err(e) = template::validate_root(&r) {
                                self.errors.error(
                                    "step.params_template_invalid",
                                    format!("{step_path}/params"),
                                    e.to_string(),
                                );
                                continue;
                            }
                            for target in template::referenced_step_names(&r) {
                                if !self.seen_step_names.contains(&target) {
                                    self.errors.error(
                                        "step.params_forward_ref",
                                        format!("{step_path}/params"),
                                        format!("tool step `{name}` params reference step `{target}` before it has run"),
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => self.errors.error(
                        "step.params_template_invalid",
                        format!("{step_path}/params"),
                        e.to_string(),
                    ),
                }
            }
            WorkflowStepDef::Wait {
                provider,
                resume_condition,
                outputs,
                ..
            } => {
                if provider.is_empty() {
                    self.errors.error(
                        "step.wait_provider_empty",
                        format!("{step_path}/provider"),
                        "`provider` must be non-empty",
                    );
                }
                match template::parse_resume_condition(resume_condition.trim()) {
                    Ok(r) => self.record_forward_step_refs(
                        &r,
                        "step.resume_forward_ref",
                        &format!("{step_path}/resume_condition"),
                        "resume_condition",
                    ),
                    Err(e) => self.errors.error(
                        "step.resume_condition_invalid",
                        format!("{step_path}/resume_condition"),
                        e.to_string(),
                    ),
                }
                for (key, expr) in outputs {
                    match template::parse_resume_condition(expr.trim()) {
                        Ok(r) => {
                            for target in template::referenced_step_names(&r) {
                                if !self.seen_step_names.contains(&target) {
                                    self.errors.error(
                                        "step.wait_output_forward_ref",
                                        format!("{step_path}/outputs/{key}"),
                                        format!(
                                            "wait output `{key}` references step `{target}` before it has run"
                                        ),
                                    );
                                }
                            }
                        }
                        Err(e) => self.errors.error(
                            "step.wait_output_invalid",
                            format!("{step_path}/outputs/{key}"),
                            e.to_string(),
                        ),
                    }
                }
            }
        }

        if !self.seen_step_names.insert(step.name().to_string()) {
            self.errors.error(
                "step.duplicate_name",
                format!("{step_path}/name"),
                format!("step name `{}` is declared more than once", step.name()),
            );
        }
    }

    pub(crate) fn record_tool_step_availability<E: std::fmt::Display>(
        &mut self,
        index: usize,
        tool: &str,
        result: Result<(), E>,
    ) {
        if let Err(e) = result {
            self.errors.error(
                "step.tool_unavailable",
                format!("/steps/{index}/tool"),
                format!("tool `{tool}` cannot be used in a workflow tool_step: {e}"),
            );
        }
    }

    pub(crate) fn record_skill_missing(&mut self, index: usize, step: &str, skill: &str) {
        self.errors.error(
            "step.skill_missing",
            format!("/steps/{index}/skill"),
            format!("step `{step}` references missing skill `{skill}`"),
        );
    }

    pub(crate) fn record_skill_lookup_error(
        &mut self,
        index: usize,
        step: &str,
        skill: &str,
        error: impl std::fmt::Display,
    ) {
        self.errors.error(
            "step.skill_lookup_error",
            format!("/steps/{index}/skill"),
            format!("step `{step}` skill `{skill}` lookup failed: {error}"),
        );
    }

    pub(crate) fn finish(self) -> WorkflowValidationErrors {
        self.errors
    }

    fn record_forward_step_refs(
        &mut self,
        r: &template::TemplateRef,
        code: &'static str,
        path: &str,
        label: &str,
    ) {
        for target in template::referenced_step_names(r) {
            if !self.seen_step_names.contains(&target) {
                self.errors.error(
                    code,
                    path,
                    format!("{label} references step `{target}` before it has run"),
                );
            }
        }
    }
}
