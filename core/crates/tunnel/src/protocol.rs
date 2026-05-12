use rmcp::model::{CallToolResult, JsonObject};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TunnelError(pub String);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelMessage {
    Register {
        deployment_id: String,
        toolsets: Vec<RegisteredToolSet>,
    },
    CallTool {
        id: String,
        upstream: String,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    },
    CallToolResult {
        id: String,
        result: serde_json::Value,
    },
    CallToolError {
        id: String,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredToolSet {
    pub name: String,
    pub prefix: String,
    pub category: String,
    pub category_description: String,
    pub tools: Vec<serde_json::Value>,
}

#[derive(Clone)]
pub enum InternalAuth {
    SharedSecret { secret: String },
    Disabled,
}

impl InternalAuth {
    pub fn header_value(&self) -> Result<String, TunnelError> {
        match self {
            InternalAuth::SharedSecret { secret } => Ok(format!("Bearer {secret}")),
            InternalAuth::Disabled => Err(TunnelError(
                "internal auth disabled — cross-pod tunnel calls unavailable".to_string(),
            )),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct InternalCallReq<'a> {
    pub upstream: &'a str,
    pub tool_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
}

pub(crate) type PendingResult = Result<CallToolResult, String>;
