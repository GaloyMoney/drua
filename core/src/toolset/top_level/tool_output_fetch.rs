//! Recovery handle for the universal-pipeline envelope.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::audit::Audit;
use crate::auth::AuthSubject;
use drua_tool_cache::ToolInvocationId;

const FETCH_MAX_UNQUERIED_BYTES: usize = 16_384;

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
    invocation_id: uuid::Uuid,
    #[serde(default)]
    view: FetchView,
    #[serde(default)]
    query: Option<FetchQuery>,
}

#[derive(Debug, Default, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum FetchView {
    /// Upstream tool's `structured_content` verbatim.
    #[default]
    Original,
    /// Typed classifier summary instead of the full output.
    Summary,
}

static INPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FetchInput>);
static OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FetchResult>);

const DESCRIPTION: &str = "Fetch a previously-persisted tool result. Same response shape \
     as calling the original tool: `structured_content` carries the \
     original tool's verbatim structured output. \
     Call shape: `{invocation_id, view?, query?}`. \
     `view: 'original'` (default) returns the upstream tool's \
     structured_content; `view: 'summary'` returns the typed \
     classifier summary instead — always succeeds with no `query` \
     (summary is bounded). \
     `query` is optional. Two axes: \
     (1) WHAT TEXT BODY to operate on — text-body modes accept an optional \
     `path` (default = whole `raw_text`; `$.foo.bar` addresses a string \
     within `structured_content`). Line numbers (`-n`) are relative to \
     the resolved body. \
     (2) HOW TO SLICE — `tail`/`head` (lines), `range` (offset+len bytes), \
     `grep` (pattern + rg-style flags), or structural navigation \
     (`json_path`/`json_array_slice`) which replace `structured_content` \
     with the slice. \
     Per-mode args: `tail`/`head` take `lines`; `range` takes `offset` + `len`; \
     `grep` takes `pattern` plus `-i`/`-A`/`-B`/`-C`/`-n`/`invert_match`/`head_limit`; \
     `json_path` takes `path` (e.g. `$.data.rows`); `json_array_slice` \
     takes `path` + `offset` + `len`. \
     The `_recover` template in `elided_paths[]._recover` (or inline on \
     array/object sentinels) gives you ready-made args.";

#[async_trait::async_trait]
impl TopLevelTool for ToolOutputFetch {
    fn name(&self) -> &str {
        "tool_output_fetch"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> &serde_json::Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        crate::toolset::invocation_owner(subject).is_some()
    }

    fn bypass_universal_pipeline(&self) -> bool {
        // Recovery output must not be re-classified or it loops on itself.
        true
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Audit::record_action("tool_output_fetch");
        let input: FetchInput = parse_params(arguments)?;
        let id = ToolInvocationId::from(input.invocation_id);

        // Cursor #3208271640: defense-in-depth ownership check.
        let Some(subject_owner) = crate::toolset::invocation_owner(subject) else {
            return Ok(CallToolResult::error(vec![Content::text(
                "tool_output_fetch failed: subject has no fetch scope".to_string(),
            )]));
        };

        let invocation = match self.tool_invocations.find_by_id(id).await {
            Ok(inv) => inv,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "tool_output_fetch failed: {e}"
                ))]));
            }
        };

        if !invocation.owner.matches(&subject_owner) {
            return Ok(CallToolResult::error(vec![Content::text(
                "tool_output_fetch failed: invocation_id is not in your scope".to_string(),
            )]));
        }

        let invocation_id_str = uuid::Uuid::from(invocation.id).to_string();
        let view_structured = match input.view {
            FetchView::Original => invocation.original_structured.clone(),
            FetchView::Summary => {
                let mut s = invocation.summary.clone();
                crate::toolset::tool_invocations::substitute_recovery_placeholder(
                    &mut s,
                    &invocation_id_str,
                );
                Some(s)
            }
        };

        let query_outcome = match input.query.as_ref() {
            Some(q) => {
                match crate::toolset::tool_invocations::apply_fetch_query(
                    &invocation.raw_text,
                    invocation.original_structured.as_ref(),
                    q,
                ) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "tool_output_fetch failed: {e}"
                        ))]));
                    }
                }
            }
            None => None,
        };

        let content_text = match (&query_outcome, input.view) {
            (Some(r), _) => r.content.clone(),
            (None, FetchView::Summary) => view_structured
                .as_ref()
                .map(|s| serde_json::to_string_pretty(s).unwrap_or_default())
                .unwrap_or_default(),
            (None, FetchView::Original) => {
                let total = invocation.raw_text.len();
                if total > FETCH_MAX_UNQUERIED_BYTES {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "tool_output_fetch refused: invocation has {total} bytes \
                         (no-query limit {FETCH_MAX_UNQUERIED_BYTES}). Either: \
                         (a) call with `view: \"summary\"` for the typed classifier \
                         summary (always succeeds, bounded size); \
                         (b) call with `query`: \
                         `tail`/`head`/`range`/`grep` for text slices, or \
                         `json_path`/`json_array_slice` for structured slices."
                    ))]));
                }
                invocation.raw_text.clone()
            }
        };

        // Any `query` (text or json) overrides structured_content with the
        // slice. JSON modes carry a real Value override; text modes (tail/
        // head/range/grep) become Value::String of the sliced text. The
        // wrap step (`wrap_non_record`) lifts non-object slice results into
        // the same `{value|items, _shape}` envelope reify uses on the input
        // side, so the MCP transport's record-only `structuredContent`
        // contract holds for every recovery template the gateway emits.
        // Objects pass through unchanged.
        //
        // Without `query`, structured_content reflects the requested view
        // (full original / typed summary) — already a record post-reify.
        let final_structured = match &query_outcome {
            Some(r) => Some(drua_tool_classifier::wrap_non_record(
                r.structured
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::String(r.content.clone())),
            )),
            None => view_structured,
        };

        let mut ctr = CallToolResult::success(vec![Content::text(content_text)]);
        ctr.structured_content = final_structured;
        Ok(ctr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_input_rejects_unknown_top_level_field() {
        let raw = serde_json::json!({
            "invocation_id": "00000000-0000-0000-0000-000000000000",
            "bogus": "should be rejected",
        });
        let err = serde_json::from_value::<FetchInput>(raw)
            .expect_err("deny_unknown_fields must reject typo'd top-level keys");
        assert!(err.to_string().to_lowercase().contains("unknown"));
    }

    #[test]
    fn fetch_input_minimal_shape_no_query_no_view() {
        let raw = serde_json::json!({
            "invocation_id": "00000000-0000-0000-0000-000000000000",
        });
        let parsed: FetchInput = serde_json::from_value(raw).expect("minimal shape must parse");
        assert!(parsed.query.is_none());
        assert!(matches!(parsed.view, FetchView::Original));
    }

    #[test]
    fn fetch_input_accepts_view_summary() {
        let raw = serde_json::json!({
            "invocation_id": "00000000-0000-0000-0000-000000000000",
            "view": "summary",
        });
        let parsed: FetchInput = serde_json::from_value(raw).expect("view: summary must parse");
        assert!(matches!(parsed.view, FetchView::Summary));
    }

    #[test]
    fn fetch_input_accepts_query_with_view() {
        let raw = serde_json::json!({
            "invocation_id": "00000000-0000-0000-0000-000000000000",
            "view": "original",
            "query": { "mode": "tail", "lines": 10 },
        });
        let parsed: FetchInput = serde_json::from_value(raw).expect("view + query parse");
        assert!(matches!(parsed.view, FetchView::Original));
        assert!(matches!(
            parsed.query,
            Some(FetchQuery::Tail { lines: 10, .. })
        ));
    }

    #[test]
    fn description_documents_view_and_each_query_mode() {
        assert!(DESCRIPTION.contains("view"));
        assert!(DESCRIPTION.contains("query"));
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
            (
                "json_path",
                serde_json::json!({"mode": "json_path", "path": "$.foo"}),
            ),
            (
                "json_array_slice",
                serde_json::json!({
                    "mode": "json_array_slice",
                    "path": "$.hits",
                    "offset": 3,
                    "len": 50,
                }),
            ),
        ] {
            assert!(DESCRIPTION.contains(&format!("`{mode}`")));
            let raw = serde_json::json!({
                "invocation_id": "00000000-0000-0000-0000-000000000000",
                "query": sample_args,
            });
            serde_json::from_value::<FetchInput>(raw)
                .unwrap_or_else(|e| panic!("description-shaped `{mode}` request must parse: {e}"));
        }
    }
}
