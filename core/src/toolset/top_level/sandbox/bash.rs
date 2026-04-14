//! `bash` — execute shell commands in the sandbox environment.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;

use super::super::super::error::ToolSetsError;
use super::super::super::traits::TopLevelTool;
use super::SandboxClient;

pub struct SandboxBash {
    client: SandboxClient,
}

impl SandboxBash {
    pub fn new(sandbox_url: String) -> Self {
        Self {
            client: SandboxClient::new(sandbox_url),
        }
    }
}

static BASH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The bash command to execute."
            },
            "restart": {
                "type": "boolean",
                "description": "If true, restart the bash session (clearing any state). Mutually exclusive with command."
            }
        },
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for SandboxBash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the sandbox environment. Returns stdout/stderr. \
         Use restart=true to reset the session."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &BASH_SCHEMA
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let input = arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let (output, is_error) = self.client.execute("bash", &input).await?;

        let mut result = CallToolResult::success(vec![Content::text(output)]);
        result.is_error = Some(is_error);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_schema_is_valid_json_schema() {
        let tool = SandboxBash::new("http://unused".into());
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["command"].is_object());
        assert!(schema["properties"]["restart"].is_object());
    }

    #[test]
    fn bash_name_and_description() {
        let tool = SandboxBash::new("http://unused".into());
        assert_eq!(tool.name(), "bash");
        assert!(!tool.description().is_empty());
    }
}
