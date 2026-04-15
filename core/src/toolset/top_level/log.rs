//! `workspace_log` and `all_logs` — audit log query tools.
//!
//! `workspace_log` returns entries scoped to the caller's workspace and
//! requires the `WorkspaceRead` scope.  It automatically excludes the
//! calling agent's own entries so an agent doesn't see its own tool calls.
//!
//! `all_logs` is admin-only and returns entries across all workspaces.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use crate::audit::{Audit, AuditEntry, AuditLogQuery};
use crate::auth::AuthSubject;

// ---------------------------------------------------------------------------
// workspace_log
// ---------------------------------------------------------------------------

pub struct WorkspaceLog {
    audit: Arc<Audit>,
}

impl WorkspaceLog {
    pub fn new(audit: Arc<Audit>) -> Self {
        Self { audit }
    }
}

static WORKSPACE_LOG_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": common_schema_properties(),
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceLog {
    fn name(&self) -> &str {
        "query_workspace_audit_log"
    }

    fn description(&self) -> &str {
        "Query audit log entries for the caller's workspace. \
         Automatically excludes the calling agent's own entries."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WORKSPACE_LOG_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_read_workspace()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.can_read_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;

        let mut query = parse_query(&arguments);
        query.workspace_id = Some(workspace_id);
        query.exclude_agent_id = subject.acting_agent_id();

        let entries = self.audit.find(&query).await?;

        Ok(CallToolResult::success(vec![Content::text(
            format_entries(&entries),
        )]))
    }
}

// ---------------------------------------------------------------------------
// all_logs
// ---------------------------------------------------------------------------

pub struct AdminAllLogs {
    audit: Arc<Audit>,
}

impl AdminAllLogs {
    pub fn new(audit: Arc<Audit>) -> Self {
        Self { audit }
    }
}

static ALL_LOGS_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": common_schema_properties(),
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for AdminAllLogs {
    fn name(&self) -> &str {
        "admin_query_audit_log"
    }

    fn description(&self) -> &str {
        "Query audit log entries across all workspaces."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ALL_LOGS_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let query = parse_query(&arguments);
        let entries = self.audit.find(&query).await?;

        Ok(CallToolResult::success(vec![Content::text(
            format_entries(&entries),
        )]))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format audit entries as a human-/LLM-readable text table.
fn format_entries(entries: &[AuditEntry]) -> String {
    if entries.is_empty() {
        return "No audit entries found.".to_string();
    }

    let mut lines = Vec::with_capacity(entries.len() + 2);
    lines.push(format!(
        "{:<6} {:<20} {:<10} {:<30} {:<8} {:>8}",
        "ID", "TIME", "TYPE", "ACTION", "OUTCOME", "MS"
    ));
    lines.push("-".repeat(86));

    for e in entries {
        let ts = e.recorded_at.format("%Y-%m-%d %H:%M:%S");
        let dur = e.duration_ms.map(|ms| format!("{ms}")).unwrap_or_default();
        lines.push(format!(
            "{:<6} {:<20} {:<10} {:<30} {:<8} {:>8}",
            e.id,
            ts,
            truncate(&e.interaction_type, 10),
            truncate(&e.action, 30),
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

/// Parse common filter fields from tool arguments into an [`AuditLogQuery`].
fn parse_query(arguments: &Option<JsonObject>) -> AuditLogQuery {
    let args = arguments.as_ref();

    let limit = args
        .and_then(|a| a.get("limit"))
        .and_then(|v| v.as_i64())
        .map(|n| n.clamp(1, 100))
        .unwrap_or(20);

    let action = parse_ilike_field(args, "action");
    let outcome = parse_ilike_field(args, "outcome");

    let acting_user_id = parse_uuid_field(args, "user_id");
    let acting_agent_id = parse_uuid_field(args, "agent_id");
    let sandbox_id = parse_uuid_field(args, "sandbox_id");

    let error = args
        .and_then(|a| a.get("errors_only"))
        .and_then(|v| v.as_bool())
        .and_then(|b| b.then_some(true));

    AuditLogQuery {
        limit,
        action,
        outcome,
        acting_user_id: acting_user_id.map(crate::primitives::UserId::from),
        acting_agent_id: acting_agent_id.map(crate::primitives::AgentId::from),
        sandbox_id: sandbox_id.map(crate::primitives::SandboxId::from),
        error,
        ..Default::default()
    }
}

/// Read a string field and wrap it in `%…%` for ILIKE matching.
fn parse_ilike_field(args: Option<&JsonObject>, key: &str) -> Option<String> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"))
}

/// Read a UUID string field.
fn parse_uuid_field(args: Option<&JsonObject>, key: &str) -> Option<uuid::Uuid> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
}

/// Shared input properties exposed by both tools.
fn common_schema_properties() -> serde_json::Value {
    serde_json::json!({
        "action": {
            "type": "string",
            "description": "Substring filter on action (e.g. 'mcp', 'POST /workspaces')."
        },
        "outcome": {
            "type": "string",
            "description": "Substring filter on outcome (e.g. 'success', 'error')."
        },
        "errors_only": {
            "type": "boolean",
            "description": "When true, return only entries that resulted in an error."
        },
        "user_id": {
            "type": "string",
            "format": "uuid",
            "description": "Filter by acting user ID."
        },
        "agent_id": {
            "type": "string",
            "format": "uuid",
            "description": "Filter by acting agent ID."
        },
        "sandbox_id": {
            "type": "string",
            "format": "uuid",
            "description": "Filter by sandbox ID."
        },
        "limit": {
            "type": "integer",
            "description": "Max entries to return (1-100, default 20)."
        }
    })
}
