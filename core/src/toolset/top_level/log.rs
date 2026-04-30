//! `project_log` — audit log query tool.
//!
//! Returns entries scoped to the caller's project and requires the
//! `ProjectAdmin` scope.  Automatically excludes the calling agent's
//! own entries so an agent doesn't see its own tool calls.
//!
//! The admin variant (`all_logs`) lives in [`super::super::searchable::admin`].

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};
use crate::audit::{Audit, AuditEntry, AuditLogQuery};
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::{AgentId, SandboxId, UserId};

#[derive(Deserialize, schemars::JsonSchema)]
struct AuditLogParams {
    entrypoint: Option<String>,
    action: Option<String>,
    outcome: Option<String>,
    errors_only: Option<bool>,
    #[schemars(with = "Option<uuid::Uuid>")]
    user_id: Option<UserId>,
    #[schemars(with = "Option<uuid::Uuid>")]
    agent_id: Option<AgentId>,
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,
    #[serde(
        default = "default_limit",
        deserialize_with = "super::liberal::deserialize_i64"
    )]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

impl AuditLogParams {
    fn into_query(self) -> AuditLogQuery {
        let entrypoint = self
            .entrypoint
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let action = self
            .action
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let outcome = self
            .outcome
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let error = self.errors_only.and_then(|b| b.then_some(true));

        AuditLogQuery {
            limit: self.limit.clamp(1, 100),
            entrypoint,
            action,
            outcome,
            acting_user_id: self.user_id,
            acting_agent_id: self.agent_id,
            sandbox_id: self.sandbox_id,
            error,
            ..Default::default()
        }
    }
}

pub struct ProjectLog {
    audit: Arc<Audit>,
}

impl ProjectLog {
    pub fn new(audit: Arc<Audit>) -> Self {
        Self { audit }
    }
}

static PROJECT_LOG_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AuditLogParams>);

#[derive(serde::Serialize, schemars::JsonSchema)]
struct AuditLogOutput {
    entries: Vec<AuditEntryOutput>,
    count: usize,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct AuditEntryOutput {
    id: i64,
    /// ISO 8601 timestamp.
    recorded_at: String,
    interaction_type: String,
    entrypoint: String,
    action: String,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<bool>,
    /// Error message text for failed interactions (truncated to 4 KiB).
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acting_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acting_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_behalf_of_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_ids: Option<serde_json::Value>,
}

impl From<&AuditEntry> for AuditEntryOutput {
    fn from(e: &AuditEntry) -> Self {
        let resource_ids = match &e.resource_ids {
            serde_json::Value::Object(m) if !m.is_empty() => {
                Some(serde_json::Value::Object(m.clone()))
            }
            _ => None,
        };
        Self {
            id: i64::from(e.id),
            recorded_at: e.recorded_at.to_rfc3339(),
            interaction_type: e.interaction_type.clone(),
            entrypoint: e.entrypoint.clone().unwrap_or_default(),
            action: e.action.clone(),
            outcome: e.outcome.clone(),
            error: e.error,
            error_message: e.error_message.clone(),
            duration_ms: e.duration_ms,
            acting_user_id: e.acting_user_id.map(|id| id.to_string()),
            acting_agent_id: e.acting_agent_id.map(|id| id.to_string()),
            on_behalf_of_user_id: e.on_behalf_of_user_id.map(|id| id.to_string()),
            resource_ids,
        }
    }
}

static PROJECT_LOG_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AuditLogOutput>);

#[async_trait::async_trait]
impl TopLevelTool for ProjectLog {
    fn name(&self) -> &str {
        "log"
    }

    fn description(&self) -> &str {
        "Query audit log entries for the caller's project. \
         Automatically excludes the calling agent's own entries."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &PROJECT_LOG_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&PROJECT_LOG_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Read, AuthResource::AuditLog(project))
                .is_ok()
        })
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
        // `Audit` has no per-subject service API, so authz lives here.
        subject
            .can(AuthVerb::Read, AuthResource::AuditLog(project_id))
            .map_err(|_| ToolSetsError::Unauthorized)?;
        Audit::record_action("audit.query");
        let params: AuditLogParams = parse_params(arguments)?;

        let mut query = params.into_query();
        query.project_id = Some(project_id);
        query.exclude_agent_id = subject.acting_agent_id();

        let entries = self.audit.find(&query).await?;

        let out = AuditLogOutput {
            count: entries.len(),
            entries: entries.iter().map(AuditEntryOutput::from).collect(),
        };
        let structured = serde_json::to_value(&out).expect("AuditLogOutput serialization");

        let mut result = CallToolResult::success(vec![Content::text(format_entries(&entries))]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

fn format_entries(entries: &[AuditEntry]) -> String {
    if entries.is_empty() {
        return "No audit entries found.".to_string();
    }

    let mut lines = Vec::with_capacity(entries.len() + 2);
    lines.push(format!(
        "{:<6} {:<20} {:<10} {:<26} {:<26} {:<8} {:>8}",
        "ID", "TIME", "TYPE", "ENTRYPOINT", "ACTION", "OUTCOME", "MS"
    ));
    lines.push("-".repeat(108));

    for e in entries {
        let ts = e.recorded_at.format("%Y-%m-%d %H:%M:%S");
        let dur = e.duration_ms.map(|ms| format!("{ms}")).unwrap_or_default();
        let ep = e.entrypoint.as_deref().unwrap_or("");
        lines.push(format!(
            "{:<6} {:<20} {:<10} {:<26} {:<26} {:<8} {:>8}",
            e.id,
            ts,
            truncate(&e.interaction_type, 10),
            truncate(ep, 26),
            truncate(&e.action, 26),
            truncate(&e.outcome, 8),
            dur,
        ));
    }

    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
