//! `whoami` — returns the current auth subject type, scopes, and identity.
//! Useful for debugging visibility / authorization issues.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct WhoAmI;

impl WhoAmI {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WhoAmI {
    fn default() -> Self {
        Self::new()
    }
}

static WHOAMI_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WhoAmI {
    fn name(&self) -> &str {
        "whoami"
    }

    fn description(&self) -> &str {
        "Returns the current authentication subject: identity type, scopes, and associated IDs. Useful for debugging tool visibility and authorization."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WHOAMI_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        matches!(subject, AuthSubject::ExportedAgent(_, _, _))
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let mut info = serde_json::Map::new();

        match subject {
            AuthSubject::User(user_id) => {
                info.insert("type".into(), "user".into());
                info.insert("user_id".into(), user_id.to_string().into());
                info.insert("note".into(), "Users implicitly have all scopes".into());
            }
            AuthSubject::ExportedAgent(user_id, creds_id, scopes) => {
                info.insert("type".into(), "exported_agent".into());
                info.insert("user_id".into(), user_id.to_string().into());
                info.insert("creds_id".into(), creds_id.to_string().into());
                info.insert(
                    "scopes".into(),
                    scopes
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .into(),
                );
            }
            AuthSubject::Agent(agent_id, scopes) => {
                info.insert("type".into(), "agent".into());
                if let Some(workspace_id) = subject.workspace_id() {
                    info.insert("workspace_id".into(), workspace_id.to_string().into());
                }
                info.insert("agent_id".into(), agent_id.to_string().into());
                info.insert(
                    "scopes".into(),
                    scopes
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .into(),
                );
            }
            AuthSubject::AgentOnBehalfOfUser(user_id, agent_id, scopes) => {
                info.insert("type".into(), "agent_on_behalf_of_user".into());
                info.insert("user_id".into(), user_id.to_string().into());
                if let Some(workspace_id) = subject.workspace_id() {
                    info.insert("workspace_id".into(), workspace_id.to_string().into());
                }
                info.insert("agent_id".into(), agent_id.to_string().into());
                info.insert(
                    "scopes".into(),
                    scopes
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .into(),
                );
            }
            AuthSubject::Anonymous => {
                info.insert("type".into(), "anonymous".into());
            }
        }

        let text = serde_json::to_string_pretty(&info).unwrap_or_else(|_| format!("{info:?}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}
