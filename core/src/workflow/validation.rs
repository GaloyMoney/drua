use serde::Serialize;

use super::definition::{WorkflowStepDef, WorkflowTrigger};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowDiagnosticSeverity {
    Error,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkflowDiagnostic {
    pub(crate) severity: WorkflowDiagnosticSeverity,
    pub(crate) code: &'static str,
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Default)]
pub(crate) struct WorkflowDiagnostics {
    pub(crate) items: Vec<WorkflowDiagnostic>,
}

impl WorkflowDiagnostics {
    pub(crate) fn push(
        &mut self,
        severity: WorkflowDiagnosticSeverity,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.items.push(WorkflowDiagnostic {
            severity,
            code,
            path: path.into(),
            message: message.into(),
        });
    }

    pub(crate) fn error(
        &mut self,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(WorkflowDiagnosticSeverity::Error, code, path, message);
    }

    pub(crate) fn info(
        &mut self,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(WorkflowDiagnosticSeverity::Info, code, path, message);
    }

    pub(crate) fn counts(&self) -> (usize, usize) {
        self.items.iter().fold((0, 0), |mut acc, d| {
            match d.severity {
                WorkflowDiagnosticSeverity::Error => acc.0 += 1,
                WorkflowDiagnosticSeverity::Info => acc.1 += 1,
            }
            acc
        })
    }
}

pub(crate) fn validate_trigger_definition(
    trigger: &WorkflowTrigger,
    diagnostics: &mut WorkflowDiagnostics,
) {
    if let WorkflowTrigger::Cron {
        schedule, timezone, ..
    } = trigger
    {
        if let Err(e) = super::parse_cron_schedule(schedule) {
            diagnostics.error("trigger.cron_invalid", "/trigger/schedule", e);
        }
        if let Err(e) = super::parse_timezone(timezone.as_deref()) {
            diagnostics.error("trigger.timezone_invalid", "/trigger/timezone", e);
        }
    }

    let Some(condition) = trigger.condition() else {
        diagnostics.info(
            "trigger.condition_absent",
            "/trigger/condition",
            "trigger is ungated",
        );
        return;
    };
    if let Err(e) = template::parse_trigger_condition(condition) {
        diagnostics.error(
            "trigger.condition_invalid",
            "/trigger/condition",
            e.to_string(),
        );
    }
}

pub(crate) fn validate_step_static(
    step: &WorkflowStepDef,
    index: usize,
    seen_step_names: &std::collections::HashSet<String>,
    diagnostics: &mut WorkflowDiagnostics,
) {
    let step_path = format!("/steps/{index}");
    if !is_cel_identifier(step.name()) {
        diagnostics.error(
            "step.name_not_cel_identifier",
            format!("{step_path}/name"),
            "step name must be a CEL identifier so downstream `steps.<name>.outputs` references can resolve",
        );
    }
    if let Some(body) = step.condition() {
        match template::parse_condition(body) {
            Ok(r) => record_forward_step_refs(
                &r,
                seen_step_names,
                "step.condition_forward_ref",
                &format!("{step_path}/condition"),
                "condition",
                diagnostics,
            ),
            Err(e) => diagnostics.error(
                "step.condition_invalid",
                format!("{step_path}/condition"),
                e.to_string(),
            ),
        }
    }

    match step {
        WorkflowStepDef::AgentStep { .. } => {}
        WorkflowStepDef::ToolStep { name, params, .. } => {
            match template::extract_refs_in_value(params) {
                Ok(refs) => {
                    for r in refs {
                        if let Err(e) = template::validate_root(&r) {
                            diagnostics.error(
                                "step.params_template_invalid",
                                format!("{step_path}/params"),
                                e.to_string(),
                            );
                            continue;
                        }
                        for target in template::referenced_step_names(&r) {
                            if !seen_step_names.contains(&target) {
                                diagnostics.error(
                                    "step.params_forward_ref",
                                    format!("{step_path}/params"),
                                    format!("tool step `{name}` params reference step `{target}` before it has run"),
                                );
                            }
                        }
                    }
                }
                Err(e) => diagnostics.error(
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
                diagnostics.error(
                    "step.wait_provider_empty",
                    format!("{step_path}/provider"),
                    "`provider` must be non-empty",
                );
            }
            match template::parse_resume_condition(resume_condition.trim()) {
                Ok(r) => record_forward_step_refs(
                    &r,
                    seen_step_names,
                    "step.resume_forward_ref",
                    &format!("{step_path}/resume_condition"),
                    "resume_condition",
                    diagnostics,
                ),
                Err(e) => diagnostics.error(
                    "step.resume_condition_invalid",
                    format!("{step_path}/resume_condition"),
                    e.to_string(),
                ),
            }
            for (key, expr) in outputs {
                match template::parse_resume_condition(expr.trim()) {
                    Ok(r) => {
                        for target in template::referenced_step_names(&r) {
                            if !seen_step_names.contains(&target) {
                                diagnostics.error(
                                    "step.wait_output_forward_ref",
                                    format!("{step_path}/outputs/{key}"),
                                    format!(
                                        "wait output `{key}` references step `{target}` before it has run"
                                    ),
                                );
                            }
                        }
                    }
                    Err(e) => diagnostics.error(
                        "step.wait_output_invalid",
                        format!("{step_path}/outputs/{key}"),
                        e.to_string(),
                    ),
                }
            }
        }
    }
}

fn record_forward_step_refs(
    r: &template::TemplateRef,
    seen_step_names: &std::collections::HashSet<String>,
    code: &'static str,
    path: &str,
    label: &str,
    diagnostics: &mut WorkflowDiagnostics,
) {
    for target in template::referenced_step_names(r) {
        if !seen_step_names.contains(&target) {
            diagnostics.error(
                code,
                path,
                format!("{label} references step `{target}` before it has run"),
            );
        }
    }
}

pub(crate) fn record_tool_step_availability<E: std::fmt::Display>(
    tool: &str,
    path: &str,
    result: Result<(), E>,
    diagnostics: &mut WorkflowDiagnostics,
) {
    match result {
        Ok(_) => diagnostics.info(
            "step.tool_resolved",
            path,
            format!("tool `{tool}` is registered, composable, and declares structured output"),
        ),
        Err(e) => diagnostics.error(
            "step.tool_unavailable",
            path,
            format!("tool `{tool}` cannot be used in a workflow tool_step: {e}"),
        ),
    }
}
