//! `tool_output_fetch` — recovery handle for the universal-pipeline envelope.
//!
//! Any large tool result returned by drua's dispatcher carries an
//! `invocation_id` in `structured_content.envelope`. This tool resolves
//! that handle into a slice of the persisted raw output, picked by exactly
//! one of `tail` / `head` / `range` / `grep`. The full row never returns to
//! the model context — agents who realise they need more should refine the
//! query rather than fetch wholesale.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;
use crate::primitives::ToolInvocationId;

use super::super::error::ToolSetsError;
use super::super::tool_invocations::{FetchQuery, FetchResult, ToolInvocations};
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

pub struct ToolOutputFetch {
    tool_invocations: Arc<ToolInvocations>,
}

impl ToolOutputFetch {
    pub fn new(tool_invocations: Arc<ToolInvocations>) -> Self {
        Self { tool_invocations }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FetchInput {
    /// The id surfaced as `envelope.invocation_id` on the original tool
    /// call. Use exactly the value the dispatcher returned.
    invocation_id: uuid::Uuid,
    /// Nested rather than `#[serde(flatten)]`-ed because serde
    /// documents `deny_unknown_fields` + `flatten` as unsupported —
    /// the deny becomes a silent no-op, letting typos like `tial`
    /// instead of `tail` slip through. Explicit nesting keeps the
    /// strict-mode guarantee.
    query: FetchQuery,
}

static INPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FetchInput>);
static OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FetchResult>);

/// Shape hint embedded in the universal-pipeline envelope so the agent
/// can call this tool without re-fetching its schema. Single source of
/// truth — drift between the hint and the actual `FetchInput` shape is
/// a real bug (the agent follows the hint literally and hits a
/// `deny_unknown_fields` rejection). Locked in by
/// `fetch_hint_matches_schema`.
pub(crate) const FETCH_HINT: &str =
    "tool_output_fetch({invocation_id, query: {mode: 'tail'|'head'|'range'|'grep', ...}})";

#[async_trait::async_trait]
impl TopLevelTool for ToolOutputFetch {
    fn name(&self) -> &str {
        "tool_output_fetch"
    }

    fn description(&self) -> &str {
        "Fetch elided detail from a prior tool call's persisted output. \
         A tool result includes an `invocation_id` if and only if its full \
         output is recoverable through this tool. \
         Call shape: `{invocation_id, query: { mode: tail|head|range|grep, ... }}`. \
         Per-mode args: `tail`/`head` take `lines`; `range` takes `offset` + `len`; \
         `grep` takes `pattern` + optional `context`. \
         Responses are capped — refine the query rather than asking for the \
         whole stream."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_use_agent_file_tools()
    }

    fn bypass_universal_pipeline(&self) -> bool {
        // The fetch tool exists to surface raw bytes from a previously
        // persisted invocation. Letting the dispatcher re-classify and
        // re-wrap that response would assign a *new* invocation_id to
        // detail the agent already explicitly asked for — the recovery
        // path would loop on itself.
        true
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let input: FetchInput = parse_params(arguments)?;
        let id = ToolInvocationId::from(input.invocation_id);

        match self.tool_invocations.fetch(id, input.query).await {
            Ok(result) => {
                let structured = serde_json::to_value(&result).expect("FetchResult serialization");
                let mut ctr = CallToolResult::success(vec![Content::text(result.content)]);
                ctr.structured_content = Some(structured);
                Ok(ctr)
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "tool_output_fetch failed: {e}"
            ))])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bugbot review on PR #309: `#[serde(deny_unknown_fields)]` is silently
    /// no-op'd when combined with `#[serde(flatten)]`. The fix is to nest
    /// `query` as an explicit field — these two cases lock that in.
    #[test]
    fn fetch_input_rejects_unknown_top_level_field() {
        let raw = serde_json::json!({
            "invocation_id": "00000000-0000-0000-0000-000000000000",
            "query": { "mode": "tail", "lines": 10 },
            "bogus": "should be rejected",
        });
        let err = serde_json::from_value::<FetchInput>(raw)
            .expect_err("deny_unknown_fields must reject typo'd top-level keys");
        assert!(err.to_string().to_lowercase().contains("unknown"));
    }

    #[test]
    fn fetch_input_accepts_well_formed_query() {
        let raw = serde_json::json!({
            "invocation_id": "00000000-0000-0000-0000-000000000000",
            "query": { "mode": "tail", "lines": 10 },
        });
        let parsed: FetchInput =
            serde_json::from_value(raw).expect("nested-query call shape parses");
        assert!(matches!(parsed.query, FetchQuery::Tail { lines: 10 }));
    }

    /// Bugbot review on PR #309 (medium): the envelope's `fetch_hint`
    /// previously claimed the call shape was
    /// `tool_output_fetch({invocation_id, mode: ..., ...})` (flat),
    /// but the actual schema requires `query: {mode: ..., ...}`
    /// (nested). An agent following the hint literally hit a
    /// `deny_unknown_fields` rejection. The hint and the schema must
    /// stay aligned — `FETCH_HINT` is the single source of truth that
    /// the dispatcher embeds.
    #[test]
    fn fetch_hint_matches_schema() {
        // The hint must reference the nested-`query` shape; if someone
        // changes it to a flat layout without also changing the schema
        // it's a regression.
        assert!(
            FETCH_HINT.contains("query: {"),
            "FETCH_HINT must mention the nested `query: {{...}}` envelope, got: {FETCH_HINT}",
        );

        // Each mode mentioned in the hint must round-trip through
        // `FetchInput` so an agent that mechanically substitutes
        // values into the hint never builds an invalid request.
        for (mode, sample_args) in [
            ("tail", serde_json::json!({"mode": "tail", "lines": 5})),
            ("head", serde_json::json!({"mode": "head", "lines": 5})),
            (
                "range",
                serde_json::json!({"mode": "range", "offset": 0, "len": 100}),
            ),
            (
                "grep",
                serde_json::json!({"mode": "grep", "pattern": "error"}),
            ),
        ] {
            assert!(
                FETCH_HINT.contains(&format!("'{mode}'")),
                "hint must list mode `{mode}`",
            );
            let raw = serde_json::json!({
                "invocation_id": "00000000-0000-0000-0000-000000000000",
                "query": sample_args,
            });
            serde_json::from_value::<FetchInput>(raw)
                .unwrap_or_else(|e| panic!("hint-shaped `{mode}` request must parse: {e}"));
        }
    }
}
