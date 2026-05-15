//! Pure-data shapes for a workflow definition. Persisted verbatim
//! inside `WorkflowDefinitionEvent` and snapshotted on every
//! `WorkflowRunEvent::Initialized` — wire-format changes here must
//! stay backward-compatible.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use llm::ModelChain;
use schemars::schema::{InstanceType, RootSchema, SingleOrVec};
use serde::{Deserialize, Deserializer, Serialize};

use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

/// JSON Schema for an AgentStep's structured output, validated at
/// construction time. The MCP / Anthropic + OpenAI tool-use APIs
/// require `type: "object"` at the root; tagged-union (`oneOf`/`anyOf`)
/// roots are silently rejected (memo `019dfc8c`). This wrapper makes
/// invalid roots unrepresentable.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(transparent)]
pub struct OutputSchema(RootSchema);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OutputSchemaError {
    #[error(
        "output_schema root must be `type: \"object\"`; tagged-union (oneOf/anyOf) \
         roots are rejected by MCP — wrap the union under a struct field instead"
    )]
    NotObjectRoot,
}

impl OutputSchema {
    pub fn new(schema: RootSchema) -> Result<Self, OutputSchemaError> {
        match &schema.schema.instance_type {
            Some(SingleOrVec::Single(t)) if **t == InstanceType::Object => {}
            Some(SingleOrVec::Vec(types))
                if types.len() == 1 && types[0] == InstanceType::Object => {}
            _ => return Err(OutputSchemaError::NotObjectRoot),
        }
        Ok(Self(schema))
    }

    pub fn into_root_schema(self) -> RootSchema {
        self.0
    }

    pub fn root_schema(&self) -> &RootSchema {
        &self.0
    }
}

impl<'de> Deserialize<'de> for OutputSchema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let schema = RootSchema::deserialize(deserializer)?;
        OutputSchema::new(schema).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTrigger {
    Manual {
        /// Bare CEL boolean evaluated against `trigger` before a run
        /// is created. `None` → fire as today. See `Webhook::condition`
        /// for full semantics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
    Webhook {
        /// `Some("honeycomb")` selects the `X-Honeycomb-Webhook-Token`
        /// header; `None` falls back to `Authorization: Bearer`.
        provider: Option<String>,
        secret: String,
        /// Bare CEL boolean evaluated against `trigger` (only —
        /// `steps` is rejected at parse time, no steps have run yet)
        /// before a run is created. `None` → always fire (back-compat).
        /// `Some(expr)` and `expr` evaluates to `false` → no run is
        /// created and a `workflow.trigger_filtered` audit row is
        /// written. Errors → no run + `workflow.trigger_condition_errored`
        /// audit row. Compiled and validated at workflow create/update
        /// time so authors learn syntax issues then, not on the first
        /// webhook.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
    /// Time-based scheduled execution. `schedule` is a 6- or 7-field
    /// cron expression as accepted by the `cron` crate (sec min hr dom
    /// mon dow [year]). `timezone` is an IANA tz name; `None` → UTC.
    Cron {
        schedule: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
        /// See `Webhook::condition`. Degenerate today — the cron
        /// trigger context is empty, so any reference to
        /// `trigger.payload.X` resolves to null. Useful once a
        /// `trigger.now` binding lands; permitted in the grammar
        /// today for uniformity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
}

impl WorkflowTrigger {
    /// Bare CEL boolean expression gating run creation. `None` →
    /// always fire (back-compat with definitions that predate the
    /// field).
    pub fn condition(&self) -> Option<&str> {
        match self {
            Self::Manual { condition }
            | Self::Webhook { condition, .. }
            | Self::Cron { condition, .. } => condition.as_deref(),
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Manual { .. } => "manual",
            Self::Webhook { .. } => "webhook",
            Self::Cron { .. } => "cron",
        }
    }

    /// `None` when this is not a cron trigger, or when the cron
    /// schedule has no future fire after the supplied instant.
    pub fn next_fire_at(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, String> {
        match self {
            Self::Cron {
                schedule, timezone, ..
            } => {
                let sched = parse_cron_schedule(schedule)?;
                let tz = parse_timezone(timezone.as_deref())?;
                Ok(sched
                    .after(&after.with_timezone(&tz))
                    .next()
                    .map(|dt| dt.with_timezone(&Utc)))
            }
            _ => Ok(None),
        }
    }
}

/// IANA name parses through `chrono_tz`; `None` → `UTC`. Surfaces a
/// stable error type so service-layer callers can distinguish it from
/// generic validation failures.
pub fn parse_timezone(tz: Option<&str>) -> Result<chrono_tz::Tz, String> {
    match tz {
        None => Ok(chrono_tz::UTC),
        Some(name) => {
            chrono_tz::Tz::from_str(name).map_err(|e| format!("invalid timezone '{name}': {e}"))
        }
    }
}

pub fn parse_cron_schedule(expr: &str) -> Result<cron::Schedule, String> {
    cron::Schedule::from_str(expr).map_err(|e| format!("invalid cron expression '{expr}': {e}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStepDef {
    AgentStep {
        name: String,
        skill: String,
        sandbox: Option<String>,
        /// Read or Write attach mode for the named sandbox.
        /// `None` → `Write`, preserving the original default.
        #[serde(default)]
        sandbox_mode: Option<SandboxAgentMode>,
        timeout_seconds: Option<u64>,
        /// Highest-precedence chain override; beats workflow + defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_chain: Option<ModelChain>,
        /// JSON Schema describing the structured payload the agent
        /// must submit via the synthesised `submit_output` tool. The
        /// root is guaranteed `type: "object"` (enforced by
        /// [`OutputSchema`] validation on construction/deserialization).
        /// Old workflows / inputs that omit the field hydrate as
        /// [`default_output_schema`] (`{success, reason}`). Boxed
        /// to keep the [`WorkflowStepDef`] enum's `AgentStep` and
        /// `ToolStep` variants close in size (clippy
        /// `large_enum_variant`).
        #[serde(default = "default_output_schema_boxed")]
        output_schema: Box<OutputSchema>,
        /// Bare CEL boolean expression evaluated against the same
        /// `(trigger, steps)` context as `${{ … }}` substitution.
        /// `None` → step always runs (back-compat). `Some(expr)` and
        /// `expr` evaluates to `false` → step is skipped: emits
        /// `WorkflowRunEvent::StepSkipped`, the run continues to the
        /// next step. Downstream `${{ steps.<this>.outputs.X }}` refs
        /// resolve to `null` (whole-string splice) or `""` (embedded)
        /// per the existing CEL missing-key semantics. Compiled and
        /// validated at workflow create/update time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
    /// Deterministic single-tool dispatch — no agent loop, no LLM.
    /// `params` is the raw `arguments` object handed to the named
    /// top-level MCP tool after `${{ … }}` substitution against the
    /// run's trigger context and prior step outputs. The tool is
    /// dispatched as `AuthSubject::WorkflowExecutor` with the
    /// workflow's project authority; only `composable: true` tools
    /// can be invoked. The tool's `structured_content` becomes
    /// `StepResult.output`; `is_error: true` fails the step.
    ToolStep {
        name: String,
        /// Top-level tool name, e.g. `"workflow"` for sub-workflow
        /// dispatch via `command: trigger`.
        tool: String,
        /// Pre-substitution params; `${{ … }}` references are resolved
        /// against [`crate::workflow::template::TemplateContext`]
        /// at execution time.
        #[serde(default)]
        params: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
        /// See `AgentStep::condition` — same semantics for ToolSteps.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
    },
}

impl WorkflowStepDef {
    pub fn name(&self) -> &str {
        match self {
            WorkflowStepDef::AgentStep { name, .. } => name,
            WorkflowStepDef::ToolStep { name, .. } => name,
        }
    }

    pub fn model_chain(&self) -> Option<&ModelChain> {
        match self {
            WorkflowStepDef::AgentStep { model_chain, .. } => model_chain.as_ref(),
            WorkflowStepDef::ToolStep { .. } => None,
        }
    }

    /// AgentSteps carry an explicit `output_schema` enforced via the
    /// synthesised `submit_output` tool. ToolSteps have none — the
    /// called tool's own declared schema is the contract.
    pub fn output_schema(&self) -> Option<&OutputSchema> {
        match self {
            WorkflowStepDef::AgentStep { output_schema, .. } => Some(output_schema.as_ref()),
            WorkflowStepDef::ToolStep { .. } => None,
        }
    }

    /// Bare CEL boolean expression that gates step execution. `None`
    /// → step always runs.
    pub fn condition(&self) -> Option<&str> {
        match self {
            WorkflowStepDef::AgentStep { condition, .. }
            | WorkflowStepDef::ToolStep { condition, .. } => condition.as_deref(),
        }
    }
}

/// Boxed factory used by `serde(default = …)` on the `AgentStep` variant.
fn default_output_schema_boxed() -> Box<OutputSchema> {
    Box::new(default_output_schema())
}

/// Default `output_schema` injected when an `AgentStep` doesn't declare
/// one. Forces every workflow agent to terminate with at least
/// `{success, output}`; `reason` is optional and used on failure to
/// cite evidence. `output` carries the step's actual deliverable.
pub fn default_output_schema() -> OutputSchema {
    let value = serde_json::json!({
        "type": "object",
        "required": ["success", "output"],
        "properties": {
            "success": {
                "type": "boolean",
                "description": "Did the step achieve its goal?"
            },
            "reason": {
                "type": "string",
                "description": "Optional one-paragraph meta-explanation of the outcome — typically used on failure to cite evidence."
            },
            "output": {
                "type": "string",
                "description": "The step's actual deliverable — the primary artifact, finding, or content the step produced. Distinct from `reason`, which is the meta-explanation."
            }
        }
    });
    let schema: RootSchema =
        serde_json::from_value(value).expect("default schema is valid JSON Schema");
    OutputSchema::new(schema).expect("default schema has type=object root")
}

/// Top-level sandbox declaration on a workflow.
///
/// `Provisioned` decls are workflow-scoped: the executor brings them
/// up before the first step (fresh per run, modulo the workflow-scope
/// reuse handled by the sandbox repo) and suspends them after.
///
/// `Preexisting` decls reference an existing sandbox in the workflow's
/// project by its name (project-unique via the `(project_id,
/// name)` constraint on `sandboxes`). The executor only attaches and
/// detaches; it never creates, restarts, or suspends. The user owns
/// the sandbox lifecycle.
///
/// `type` is the discriminator on the wire (matching `WorkflowStepDef`
/// and `WorkflowTrigger` conventions in this module).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowSandboxDecl {
    Provisioned {
        name: String,
        #[serde(flatten)]
        mode: SandboxMode,
        #[serde(default)]
        specs: Option<SandboxSpecs>,
    },
    Preexisting {
        name: String,
    },
}

impl WorkflowSandboxDecl {
    pub fn name(&self) -> &str {
        match self {
            Self::Provisioned { name, .. } | Self::Preexisting { name } => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_schema_is_object_root() {
        let schema = default_output_schema();
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            value.get("type").and_then(|t| t.as_str()),
            Some("object"),
            "MCP requires `type: object` at the schema root (memo 019dfc8c)"
        );
        let required = value
            .get("required")
            .and_then(|r| r.as_array())
            .expect("default schema declares required");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"success"));
        assert!(names.contains(&"output"));
        // `reason` is optional — present in `properties` but not
        // `required`, since on success it's redundant with `output`.
        assert!(!names.contains(&"reason"));
        let properties = value
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("default schema declares properties");
        assert!(properties.contains_key("reason"));
    }

    #[test]
    fn output_schema_new_accepts_object_root() {
        let schema: RootSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "required": ["x"],
        }))
        .unwrap();
        OutputSchema::new(schema).expect("object root accepted");
    }

    #[test]
    fn output_schema_new_rejects_array_root() {
        let schema: RootSchema =
            serde_json::from_value(serde_json::json!({ "type": "array" })).unwrap();
        assert_eq!(
            OutputSchema::new(schema),
            Err(OutputSchemaError::NotObjectRoot)
        );
    }

    #[test]
    fn output_schema_new_rejects_one_of_root() {
        let schema: RootSchema = serde_json::from_value(serde_json::json!({
            "oneOf": [
                { "type": "object" },
                { "type": "object" }
            ]
        }))
        .unwrap();
        assert_eq!(
            OutputSchema::new(schema),
            Err(OutputSchemaError::NotObjectRoot)
        );
    }

    #[test]
    fn output_schema_deserialize_rejects_non_object_root() {
        let res: Result<OutputSchema, _> =
            serde_json::from_value(serde_json::json!({ "type": "array" }));
        assert!(res.is_err(), "non-object roots must fail deserialization");
    }

    #[test]
    fn output_schema_accessor_returns_declared() {
        let custom_value = serde_json::json!({
            "type": "object",
            "required": ["verdict"],
            "properties": { "verdict": {"type": "string"} }
        });
        let custom: OutputSchema = serde_json::from_value(custom_value.clone()).unwrap();
        let step = WorkflowStepDef::AgentStep {
            name: "judge".into(),
            skill: "judge".into(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: None,
            output_schema: Box::new(custom),
            condition: None,
        };
        let actual = serde_json::to_value(step.output_schema().expect("AgentStep")).unwrap();
        assert_eq!(actual, custom_value);
    }

    #[test]
    fn output_schema_defaults_when_yaml_omits_field() {
        // Tagged-enum struct format with discriminator + flat fields.
        let json = serde_json::json!({
            "type": "agent_step",
            "name": "noop",
            "skill": "noop",
            "sandbox": null,
            "sandbox_mode": null,
            "timeout_seconds": null
        });
        let step: WorkflowStepDef = serde_json::from_value(json).unwrap();
        let default = serde_json::to_value(default_output_schema()).unwrap();
        let actual = serde_json::to_value(step.output_schema().expect("AgentStep")).unwrap();
        assert_eq!(actual, default);
    }

    #[test]
    fn tool_step_round_trips_through_serde() {
        let step = WorkflowStepDef::ToolStep {
            name: "dispatch".into(),
            tool: "workflow".into(),
            params: serde_json::json!({
                "command": "trigger",
                "definition_id": "drua-test-failure",
                "payload": "${{ steps.triage.outputs.args }}"
            }),
            timeout_seconds: Some(60),
            condition: None,
        };
        let value = serde_json::to_value(&step).unwrap();
        assert_eq!(
            value.get("type").and_then(|v| v.as_str()),
            Some("tool_step")
        );
        assert_eq!(value.get("tool").and_then(|v| v.as_str()), Some("workflow"));
        let back: WorkflowStepDef = serde_json::from_value(value).unwrap();
        assert_eq!(back.name(), "dispatch");
        assert!(back.output_schema().is_none());
        assert!(back.model_chain().is_none());
    }

    /// `params` deserializes to `Value::Null` when omitted (serde
    /// default for an untagged `serde_json::Value`). The executor
    /// treats that as "no arguments" — see the
    /// `Value::Null => None` arm of the dispatch path in
    /// `executor::execute_tool_step`. The wire null is the
    /// idiomatic absent-marker; we don't rewrite it to `{}`.
    #[test]
    fn tool_step_omitted_params_deserializes_as_null() {
        let json = serde_json::json!({
            "type": "tool_step",
            "name": "noop",
            "tool": "whoami"
        });
        let step: WorkflowStepDef = serde_json::from_value(json).unwrap();
        match step {
            WorkflowStepDef::ToolStep { params, .. } => {
                assert_eq!(params, serde_json::Value::Null);
            }
            _ => panic!("expected ToolStep"),
        }
    }

    #[test]
    fn output_schema_default_round_trips_through_serde() {
        let step = WorkflowStepDef::AgentStep {
            name: "noop".into(),
            skill: "noop".into(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: None,
            output_schema: Box::new(default_output_schema()),
            condition: None,
        };
        let value = serde_json::to_value(&step).unwrap();
        assert!(
            value.get("output_schema").is_some(),
            "output_schema is always serialized; never omitted"
        );
        let back: WorkflowStepDef = serde_json::from_value(value).unwrap();
        let back_schema = serde_json::to_value(back.output_schema()).unwrap();
        let default = serde_json::to_value(default_output_schema()).unwrap();
        assert_eq!(back_schema, default);
    }

    #[test]
    fn parse_cron_schedule_accepts_six_fields() {
        // sec min hr dom mon dow — every six hours
        assert!(parse_cron_schedule("0 0 */6 * * *").is_ok());
    }

    #[test]
    fn parse_cron_schedule_rejects_garbage() {
        let err = parse_cron_schedule("not a cron expression").unwrap_err();
        assert!(err.contains("invalid cron expression"));
    }

    #[test]
    fn parse_timezone_defaults_to_utc() {
        assert_eq!(parse_timezone(None).unwrap(), chrono_tz::UTC);
    }

    #[test]
    fn parse_timezone_accepts_iana_name() {
        assert_eq!(
            parse_timezone(Some("America/New_York")).unwrap(),
            chrono_tz::America::New_York
        );
    }

    #[test]
    fn parse_timezone_rejects_garbage() {
        let err = parse_timezone(Some("Mars/Olympus_Mons")).unwrap_err();
        assert!(err.contains("invalid timezone"));
    }

    #[test]
    fn cron_trigger_next_fire_at_returns_future_time() {
        let now = chrono::Utc::now();
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 0 */6 * * *".to_string(),
            timezone: None,
            condition: None,
        };
        let next = trigger
            .next_fire_at(now)
            .unwrap()
            .expect("expression always has a next fire");
        assert!(next > now);
    }

    #[test]
    fn cron_trigger_next_fire_at_propagates_timezone_errors() {
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 0 */6 * * *".to_string(),
            timezone: Some("Not/AZone".to_string()),
            condition: None,
        };
        let err = trigger.next_fire_at(chrono::Utc::now()).unwrap_err();
        assert!(err.contains("invalid timezone"));
    }

    #[test]
    fn cron_trigger_yaml_round_trip_via_serde() {
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".to_string(),
            timezone: Some("UTC".to_string()),
            condition: None,
        };
        let s = serde_yaml::to_string(&trigger).unwrap();
        assert!(s.contains("type: cron"));
        assert!(s.contains("schedule: 0 */6 * * * *"));
        assert!(s.contains("timezone: UTC"));

        let back: WorkflowTrigger = serde_yaml::from_str(&s).unwrap();
        match back {
            WorkflowTrigger::Cron {
                schedule,
                timezone,
                condition,
            } => {
                assert_eq!(schedule, "0 */6 * * * *");
                assert_eq!(timezone.as_deref(), Some("UTC"));
                assert!(condition.is_none());
            }
            _ => panic!("expected Cron variant"),
        }
    }

    #[test]
    fn cron_trigger_yaml_omits_default_timezone() {
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".to_string(),
            timezone: None,
            condition: None,
        };
        let s = serde_yaml::to_string(&trigger).unwrap();
        assert!(!s.contains("timezone"));
    }

    #[test]
    fn manual_trigger_with_condition_round_trips() {
        let trigger = WorkflowTrigger::Manual {
            condition: Some("trigger.payload.env == 'staging'".to_string()),
        };
        let s = serde_yaml::to_string(&trigger).unwrap();
        let back: WorkflowTrigger = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back.condition(), Some("trigger.payload.env == 'staging'"));
    }

    #[test]
    fn manual_trigger_omits_absent_condition() {
        let trigger = WorkflowTrigger::Manual { condition: None };
        let s = serde_yaml::to_string(&trigger).unwrap();
        assert!(!s.contains("condition"));
    }

    /// Old serialized triggers without the `condition` field must
    /// hydrate cleanly (replay-safety on `WorkflowDefinitionEvent::Initialized`).
    #[test]
    fn legacy_trigger_without_condition_deserializes_to_none() {
        let yaml = "type: webhook\nprovider: github_app\nsecret: whsec_x\n";
        let trigger: WorkflowTrigger = serde_yaml::from_str(yaml).unwrap();
        assert!(trigger.condition().is_none());

        let yaml = "type: manual\n";
        let trigger: WorkflowTrigger = serde_yaml::from_str(yaml).unwrap();
        assert!(trigger.condition().is_none());

        let yaml = "type: cron\nschedule: '0 0 * * * *'\n";
        let trigger: WorkflowTrigger = serde_yaml::from_str(yaml).unwrap();
        assert!(trigger.condition().is_none());
    }
}
