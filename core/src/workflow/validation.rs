use std::collections::HashMap;

use serde::Serialize;

use crate::sandbox::SandboxState;

use super::definition::{OutputSchema, WorkflowStepDef, WorkflowTrigger};
use super::template::{self, ConditionOutcome, TemplateContext};

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
    Warning,
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

    pub(crate) fn warning(
        &mut self,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(WorkflowDiagnosticSeverity::Warning, code, path, message);
    }

    pub(crate) fn info(
        &mut self,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(WorkflowDiagnosticSeverity::Info, code, path, message);
    }

    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        self.items.iter().fold((0, 0, 0), |mut acc, d| {
            match d.severity {
                WorkflowDiagnosticSeverity::Error => acc.0 += 1,
                WorkflowDiagnosticSeverity::Warning => acc.1 += 1,
                WorkflowDiagnosticSeverity::Info => acc.2 += 1,
            }
            acc
        })
    }
}

pub(crate) fn lint_strict_output_schema(
    schema: &OutputSchema,
    path: &str,
    diagnostics: &mut WorkflowDiagnostics,
) {
    let value = serde_json::to_value(schema.root_schema()).expect("OutputSchema serializes");
    let normalized = crate::toolset::normalize_for_strict(value);
    lint_strict_schema_value(&normalized, path, diagnostics);
}

fn lint_strict_schema_value(
    value: &serde_json::Value,
    path: &str,
    diagnostics: &mut WorkflowDiagnostics,
) {
    match value {
        serde_json::Value::Bool(_) => diagnostics.error(
            "output_schema.boolean_schema",
            path,
            "boolean JSON Schemas are not compatible with strict provider tool schemas",
        ),
        serde_json::Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                lint_strict_schema_value(item, &format!("{path}/{idx}"), diagnostics);
            }
        }
        serde_json::Value::Object(obj) => {
            if obj.get("type").and_then(|v| v.as_str()) == Some("array") {
                match obj.get("items") {
                    Some(serde_json::Value::Object(items)) if items.is_empty() => {
                        diagnostics.error(
                            "output_schema.array_items_untyped",
                            format!("{path}/items"),
                            "array schemas must declare a concrete item schema; `items: {}` is rejected by strict provider tool validation",
                        );
                    }
                    Some(items) => {
                        lint_strict_schema_value(items, &format!("{path}/items"), diagnostics)
                    }
                    None => diagnostics.error(
                        "output_schema.array_items_missing",
                        format!("{path}/items"),
                        "array schemas must declare `items` for strict provider tool validation",
                    ),
                }
            }

            if obj.get("type").and_then(|v| v.as_str()) == Some("object") {
                if obj.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
                    diagnostics.error(
                        "output_schema.additional_properties",
                        path,
                        "strict provider tool schemas require `additionalProperties: false` on every object",
                    );
                }
                let props = obj
                    .get("properties")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                let required: std::collections::HashSet<String> = obj
                    .get("required")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for prop in props.keys() {
                    if !required.contains(prop) {
                        diagnostics.error(
                            "output_schema.required_properties",
                            format!("{path}/required"),
                            format!(
                                "strict provider tool schemas require property `{prop}` to appear in `required`"
                            ),
                        );
                    }
                }
            }

            for (key, child) in obj {
                if key == "additionalProperties" {
                    continue;
                }
                lint_strict_schema_value(child, &format!("{path}/{key}"), diagnostics);
            }
        }
        _ => {}
    }
}

pub(crate) fn validate_trigger_definition(
    trigger: &WorkflowTrigger,
    payload: Option<&serde_json::Value>,
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
        return;
    }
    if let Some(payload) = payload {
        match template::evaluate_trigger_condition(condition, payload) {
            Ok(ConditionOutcome::True) => diagnostics.info(
                "trigger.sample_true",
                "/trigger/condition",
                "sample payload passes the trigger condition",
            ),
            Ok(ConditionOutcome::False) => diagnostics.warning(
                "trigger.sample_false",
                "/trigger/condition",
                "sample payload is filtered by the trigger condition",
            ),
            Ok(ConditionOutcome::NotBoolean(value)) => diagnostics.error(
                "trigger.sample_not_boolean",
                "/trigger/condition",
                format!("sample condition evaluated to {value}, expected boolean"),
            ),
            Err(e) => diagnostics.warning(
                "trigger.sample_error",
                "/trigger/condition",
                format!("sample trigger condition could not be evaluated: {e}"),
            ),
        }
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
        WorkflowStepDef::AgentStep { output_schema, .. } => {
            lint_strict_output_schema(
                output_schema,
                &format!("{step_path}/output_schema"),
                diagnostics,
            );
        }
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

pub(crate) fn evaluate_step_routing(
    steps: &[WorkflowStepDef],
    payload: &serde_json::Value,
    diagnostics: &mut WorkflowDiagnostics,
) {
    let mut outputs = HashMap::new();
    for (index, step) in steps.iter().enumerate() {
        let path = format!("/steps/{index}/condition");
        match step.condition() {
            None => diagnostics.info(
                "routing.sample_would_run",
                path,
                format!(
                    "step `{}` has no condition and would be reached in the approximate route",
                    step.name()
                ),
            ),
            Some(body) => {
                match template::parse_condition(body) {
                    Ok(r) => {
                        let prior_refs: Vec<String> = template::referenced_step_names(&r)
                            .into_iter()
                            .filter(|name| outputs.contains_key(name))
                            .collect();
                        if !prior_refs.is_empty() {
                            diagnostics.info(
                                "routing.sample_unknown_due_to_prior_outputs",
                                path,
                                format!(
                                    "sample route for step `{}` depends on prior step output(s) {}; provide concrete prior outputs for exact routing",
                                    step.name(),
                                    prior_refs.join(", ")
                                ),
                            );
                            outputs.insert(
                                step.name().to_string(),
                                serde_json::Value::Object(Default::default()),
                            );
                            continue;
                        }
                    }
                    Err(e) => {
                        diagnostics.warning(
                            "routing.sample_unknown",
                            path,
                            format!(
                                "sample route for step `{}` could not parse: {e}",
                                step.name()
                            ),
                        );
                        outputs.insert(
                            step.name().to_string(),
                            serde_json::Value::Object(Default::default()),
                        );
                        continue;
                    }
                }
                let ctx = TemplateContext {
                    trigger: payload,
                    steps: &outputs,
                };
                match ctx.evaluate_condition(body) {
                    Ok(ConditionOutcome::True) => diagnostics.info(
                        "routing.sample_would_run",
                        path,
                        format!("sample payload makes step `{}` condition true", step.name()),
                    ),
                    Ok(ConditionOutcome::False) => diagnostics.info(
                        "routing.sample_would_skip",
                        path,
                        format!("sample payload makes step `{}` condition false", step.name()),
                    ),
                    Ok(ConditionOutcome::NotBoolean(value)) => diagnostics.warning(
                        "routing.sample_not_boolean",
                        path,
                        format!("sample condition for step `{}` evaluated to {value}, expected boolean", step.name()),
                    ),
                    Err(e) => diagnostics.warning(
                        "routing.sample_unknown",
                        path,
                        format!(
                            "sample route for step `{}` could not be evaluated without real prior step outputs: {e}",
                            step.name()
                        ),
                    ),
                }
            }
        }
        outputs.insert(
            step.name().to_string(),
            serde_json::Value::Object(Default::default()),
        );
    }
}

pub(crate) fn record_preexisting_sandbox_state(
    name: &str,
    state: SandboxState,
    path: &str,
    diagnostics: &mut WorkflowDiagnostics,
) {
    match state {
        SandboxState::Ready => {}
        SandboxState::Suspended | SandboxState::Errored => diagnostics.info(
            "sandbox.preexisting_restartable",
            path,
            format!(
                "preexisting sandbox `{name}` is currently `{state}`; workflow runtime preflight will restart dormant sandboxes before waiting for readiness"
            ),
        ),
        SandboxState::Provisioning | SandboxState::Initializing => diagnostics.warning(
            "sandbox.preexisting_not_ready",
            path,
            format!(
                "preexisting sandbox `{name}` is currently `{state}`; workflow runtime preflight will wait for readiness but validate does not wait or mutate sandbox state"
            ),
        ),
    }
}
