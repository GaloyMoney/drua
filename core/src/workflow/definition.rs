//! Pure-data shapes for a workflow definition. Persisted verbatim
//! inside `WorkflowDefinitionEvent` and snapshotted on every
//! `WorkflowRunEvent::Initialized` — wire-format changes here must
//! stay backward-compatible.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use llm::ModelChain;
use serde::{Deserialize, Serialize};

use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTrigger {
    Manual,
    Webhook {
        /// `Some("honeycomb")` selects the `X-Honeycomb-Webhook-Token`
        /// header; `None` falls back to `Authorization: Bearer`.
        provider: Option<String>,
        secret: String,
    },
    /// Time-based scheduled execution. `schedule` is a 6- or 7-field
    /// cron expression as accepted by the `cron` crate (sec min hr dom
    /// mon dow [year]). `timezone` is an IANA tz name; `None` → UTC.
    Cron {
        schedule: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
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

/// `None` when the schedule has no future fire (e.g. a one-shot
/// expression whose only date already passed).
pub fn next_cron_fire_at(
    schedule: &str,
    timezone: Option<&str>,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, String> {
    let sched = parse_cron_schedule(schedule)?;
    let tz = parse_timezone(timezone)?;
    Ok(sched
        .after(&after.with_timezone(&tz))
        .next()
        .map(|dt| dt.with_timezone(&Utc)))
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
        /// Per-step chain override. Highest precedence — wins over the
        /// workflow definition's `model_chain` and the config defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_chain: Option<ModelChain>,
    },
}

impl WorkflowStepDef {
    pub fn name(&self) -> &str {
        match self {
            WorkflowStepDef::AgentStep { name, .. } => name,
        }
    }

    /// The chain override declared on this specific step, if any.
    pub fn model_chain(&self) -> Option<&ModelChain> {
        match self {
            WorkflowStepDef::AgentStep { model_chain, .. } => model_chain.as_ref(),
        }
    }
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
    fn next_cron_fire_at_returns_future_time() {
        let now = chrono::Utc::now();
        let next = next_cron_fire_at("0 0 */6 * * *", None, now)
            .unwrap()
            .expect("expression always has a next fire");
        assert!(next > now);
    }

    #[test]
    fn next_cron_fire_at_propagates_timezone_errors() {
        let err =
            next_cron_fire_at("0 0 */6 * * *", Some("Not/AZone"), chrono::Utc::now()).unwrap_err();
        assert!(err.contains("invalid timezone"));
    }

    #[test]
    fn cron_trigger_yaml_round_trip_via_serde() {
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".to_string(),
            timezone: Some("UTC".to_string()),
        };
        let s = serde_yaml::to_string(&trigger).unwrap();
        assert!(s.contains("type: cron"));
        assert!(s.contains("schedule: 0 */6 * * * *"));
        assert!(s.contains("timezone: UTC"));

        let back: WorkflowTrigger = serde_yaml::from_str(&s).unwrap();
        match back {
            WorkflowTrigger::Cron { schedule, timezone } => {
                assert_eq!(schedule, "0 */6 * * * *");
                assert_eq!(timezone.as_deref(), Some("UTC"));
            }
            _ => panic!("expected Cron variant"),
        }
    }

    #[test]
    fn cron_trigger_yaml_omits_default_timezone() {
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".to_string(),
            timezone: None,
        };
        let s = serde_yaml::to_string(&trigger).unwrap();
        assert!(!s.contains("timezone"));
    }
}
