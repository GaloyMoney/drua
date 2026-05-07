use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::schema_for;

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

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct WhoAmIOutput {
    /// user, exported_agent, agent, agent_on_behalf_of_user, workflow_executor, or anonymous.
    #[serde(rename = "type")]
    identity_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    creds_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

static WHOAMI_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<WhoAmIOutput>);

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

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&WHOAMI_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        matches!(subject, AuthSubject::ExportedAgent(_, _, _))
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let scopes_list = |scopes: &[crate::auth::AuthScope]| -> Vec<String> {
            scopes.iter().map(|s| s.to_string()).collect()
        };

        let out = match subject {
            AuthSubject::User(user_id) => WhoAmIOutput {
                identity_type: "user".into(),
                user_id: Some(user_id.to_string()),
                note: Some("Users implicitly have all scopes".into()),
                ..Default::default()
            },
            AuthSubject::ExportedAgent(user_id, creds_id, scopes) => WhoAmIOutput {
                identity_type: "exported_agent".into(),
                user_id: Some(user_id.to_string()),
                creds_id: Some(creds_id.to_string()),
                scopes: Some(scopes_list(scopes)),
                ..Default::default()
            },
            AuthSubject::Agent(project_id, agent_id, scopes) => WhoAmIOutput {
                identity_type: "agent".into(),
                project_id: Some(project_id.to_string()),
                agent_id: Some(agent_id.to_string()),
                scopes: Some(scopes_list(scopes)),
                ..Default::default()
            },
            AuthSubject::AgentOnBehalfOfUser(user_id, project_id, agent_id, scopes) => {
                WhoAmIOutput {
                    identity_type: "agent_on_behalf_of_user".into(),
                    user_id: Some(user_id.to_string()),
                    project_id: Some(project_id.to_string()),
                    agent_id: Some(agent_id.to_string()),
                    scopes: Some(scopes_list(scopes)),
                    ..Default::default()
                }
            }
            AuthSubject::WorkflowExecutor(project_id, run_id, scopes) => WhoAmIOutput {
                identity_type: "workflow_executor".into(),
                project_id: Some(project_id.to_string()),
                workflow_run_id: Some(run_id.to_string()),
                scopes: Some(scopes_list(scopes)),
                ..Default::default()
            },
            AuthSubject::Anonymous => WhoAmIOutput {
                identity_type: "anonymous".into(),
                ..Default::default()
            },
        };

        let structured = serde_json::to_value(&out).expect("WhoAmIOutput serialization");
        let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "{}".to_string());
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}
