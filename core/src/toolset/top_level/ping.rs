//! `ping` — trivial liveness tool. Only visible and callable by an
//! `AuthSubject::ExportedAgent` so it doubles as a sanity check that an
//! exported-agent bearer token actually reached the gateway.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct Ping;

impl Ping {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Ping {
    fn default() -> Self {
        Self::new()
    }
}

static PING_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for Ping {
    fn name(&self) -> &str {
        "ping"
    }

    fn description(&self) -> &str {
        "Returns 'pong'. Only available to exported-agent credentials; \
         used as a liveness probe."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &PING_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        matches!(subject, AuthSubject::ExportedAgent(_, _, _))
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        matches!(subject, AuthSubject::ExportedAgent(_, _, _))
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }
}
