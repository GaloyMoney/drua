//! Recovery handle for the tool-caching envelope. Given an
//! `invocation_id` (advertised verbatim in `<recovery>`), navigate a
//! JSON-path on the persisted upstream payload and optionally slice
//! the resolved value.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, JsonObject};

use drua_tool_caching::{FetchQuery, ToolCaching, ToolCallOwnerId, ToolInvocationId};

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

pub struct ToolOutputFetch {
    tool_caching: Arc<ToolCaching>,
    description: String,
}

impl ToolOutputFetch {
    pub fn new(tool_caching: Arc<ToolCaching>) -> Self {
        Self {
            tool_caching,
            description: DESCRIPTION.to_string(),
        }
    }
}

const DESCRIPTION: &str = "Recover a slice of a previously-summarised tool output. \
    `invocation_id` is the uuid advertised inside the envelope's <recovery> block. \
    `path` (default `$`) is a json-path anchor against the persisted root value. \
    `query` (optional) further slices the resolved value: \
    `{mode:\"range\", offset, len}` returns bytes `[offset..offset+len]` of a string at the path; \
    `{mode:\"lines\", offset, len}` returns line range `[offset..offset+len]` of a string at the path; \
    `{mode:\"json_array_slice\", offset, len}` returns item range `arr[offset..offset+len]` of an array at the path; \
    `{mode:\"summary\"}` replays the original curated `<summary>+<recovery>` envelope (ignores `path`). \
    The response is the resolved (and optionally sliced) value, wrapped back at `path`.";

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ToolOutputFetchArgs {
    /// UUID of the invocation to recover from (from the envelope).
    invocation_id: String,
    /// JSON-path anchor; navigates into the persisted root. Defaults to `$`.
    #[serde(default = "default_path")]
    path: String,
    /// Optional slice operation applied at `path`. When absent, the
    /// whole value at `path` is returned.
    #[serde(default)]
    query: Option<FetchQuery>,
}

fn default_path() -> String {
    "$".to_string()
}

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ToolOutputFetchArgs>);

#[async_trait::async_trait]
impl TopLevelTool for ToolOutputFetch {
    fn name(&self) -> &str {
        "tool_output_fetch"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }
    fn default_tool_caching(&self) -> bool {
        // Recovery is the *output* of caching — re-caching it would loop.
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args: ToolOutputFetchArgs = parse_params(arguments)?;
        let owner_id: ToolCallOwnerId =
            <&AuthSubject as Into<Option<ToolCallOwnerId>>>::into(subject).ok_or_else(|| {
                ToolSetsError::InvalidArgument(
                    "tool_output_fetch requires a user/agent subject".into(),
                )
            })?;
        let invocation_id: ToolInvocationId = args.invocation_id.parse().map_err(|_| {
            ToolSetsError::InvalidArgument(format!("invalid invocation_id: {}", args.invocation_id))
        })?;

        let fetched = self
            .tool_caching
            .fetch(owner_id, invocation_id, &args.path, args.query.as_ref())
            .await?;
        Ok(fetched.result)
    }
}
