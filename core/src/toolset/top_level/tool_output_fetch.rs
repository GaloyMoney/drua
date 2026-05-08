//! `tool_output_fetch` — recovery handle for the universal-pipeline envelope.
//!
//! When you call this tool with an `invocation_id` from a prior tool's
//! envelope, the response **mirrors what the original tool returned**:
//! `structured_content` carries the upstream tool's verbatim
//! `structured_content` (so compose JS callers see the same Value they'd
//! get from re-running the tool fresh — full disclosure by default).
//!
//! Optional `query` slices the persisted canonical text into
//! `content[].text` for the model-facing view; agents asking for tail /
//! head / grep / range still get those bytes. The `view: "summary"` flag
//! returns the typed classifier summary as `structured_content` instead
//! of the original — useful when callers want the post-elision shape
//! without re-deriving it.
//!
//! The persisted envelopes don't ship a duplicate fetch-shape hint —
//! `tool_output_fetch` is a visible top-level tool and its
//! [`TopLevelTool::description`] is the canonical advertisement; the
//! per-row envelope just carries the `invocation_id` and a short
//! pointer at this tool in `content[].text`.

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
    /// Default `original` — the response's `structured_content` carries
    /// the upstream tool's verbatim `structured_content`, same shape an
    /// agent would receive from calling the original tool fresh. Set to
    /// `summary` to get the classifier's typed summary instead (useful
    /// when callers want the post-elision shape directly).
    #[serde(default)]
    view: FetchView,
    /// Optional slice operation on the persisted canonical text. When
    /// absent, the response's `content[].text` is empty — the typed
    /// data lives entirely in `structured_content`. When present,
    /// `content[].text` carries the slice; `structured_content`
    /// remains the original (or summary, per `view`).
    #[serde(default)]
    query: Option<FetchQuery>,
}

#[derive(Debug, Default, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum FetchView {
    /// Full disclosure: return whatever the original tool put in its
    /// `structured_content`. Compose JS callers see the same Value
    /// they'd get from re-running the tool.
    #[default]
    Original,
    /// Return the classifier's typed summary (e.g. `Concourse(...)`,
    /// `StructuredElision { kept, ... }`). Smaller payload; same shape
    /// the agent saw in the universal envelope.
    Summary,
}

static INPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FetchInput>);
static OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FetchResult>);

/// The agent-facing advertisement for this tool. Lives next to
/// `FetchInput`'s `deny_unknown_fields` so the description and the
/// accepted schema stay aligned — drift is a real bug since agents
/// follow the description literally. Locked-in by
/// `description_documents_view_and_each_query_mode`.
const DESCRIPTION: &str = "Fetch a previously-persisted tool result. Same response shape \
     as calling the original tool: `structured_content` carries the \
     original tool's verbatim structured output. \
     Call shape: `{invocation_id, view?, query?}`. \
     `view: 'original'` (default) returns the upstream tool's \
     structured_content; `view: 'summary'` returns the typed \
     classifier summary instead. \
     `query` is optional — when present, content[].text carries a \
     slice (`tail`/`head`/`range`/`grep`); when absent, no slicing. \
     Per-mode args: `tail`/`head` take `lines`; `range` takes \
     `offset` + `len`; `grep` takes `pattern` plus rg-style flags — \
     `-i` (case_insensitive), `-A`/`-B`/`-C` (context, asymmetric or \
     symmetric), `-n` (line numbers, default true), `invert_match` \
     (`-v`), and `head_limit` (cap kept lines).";

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
        // Mirror the inner `call` body's ownership gate — any subject
        // that can derive a `ToolInvocationOwner` is also entitled to
        // fetch its own persisted invocations. The previous gate
        // (`can_use_agent_file_tools`) was the *file-tool* permission
        // and required a readable sandbox; an agent without one would
        // see envelopes pointing at this tool but not be able to call
        // it.
        crate::toolset::tool_invocations::ToolInvocationOwner::from_subject(subject).is_some()
    }

    fn bypass_universal_pipeline(&self) -> bool {
        // The fetch tool exists to surface the persisted invocation
        // verbatim. Letting the dispatcher re-classify and re-wrap that
        // response would assign a *new* invocation_id to detail the
        // agent already explicitly asked for — the recovery path would
        // loop on itself.
        true
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let input: FetchInput = parse_params(arguments)?;
        let id = ToolInvocationId::from(input.invocation_id);

        // Defense-in-depth ownership check (cursor review
        // #3208271640). UUIDs are hard to guess and are only surfaced
        // to the originating agent's context, but the fetch tool
        // shouldn't rely on that. Subjects with no scope
        // (`Anonymous`, `WorkflowExecutor`) can't fetch at all;
        // others can only fetch invocations whose persisted
        // `owner` matches their derived owner.
        let Some(subject_owner) =
            crate::toolset::tool_invocations::ToolInvocationOwner::from_subject(subject)
        else {
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

        if invocation.owner != subject_owner {
            return Ok(CallToolResult::error(vec![Content::text(
                "tool_output_fetch failed: invocation_id is not in your scope".to_string(),
            )]));
        }

        // Pick the structured form per `view`. `Original` is full
        // disclosure (matches what the original tool returned);
        // `Summary` returns the typed classifier output.
        let structured = match input.view {
            FetchView::Original => invocation.original_structured.clone(),
            FetchView::Summary => Some(invocation.summary.clone()),
        };

        // Apply the optional slice query against the canonical text.
        // When no query is provided, content[].text is empty — the
        // typed data lives in structured_content, which is what
        // compose JS reads via result_to_value.
        let content_text = match input.query {
            Some(q) => {
                match crate::toolset::tool_invocations::apply_fetch_query(&invocation.raw_text, &q)
                {
                    Ok(r) => r.content,
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "tool_output_fetch failed: {e}"
                        ))]));
                    }
                }
            }
            None => String::new(),
        };

        let mut ctr = CallToolResult::success(vec![Content::text(content_text)]);
        ctr.structured_content = structured;
        Ok(ctr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bugbot review on PR #309: `#[serde(deny_unknown_fields)]` is
    /// silently no-op'd when combined with `#[serde(flatten)]`.
    /// Locked in: typos at any top-level key are rejected.
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
        // Default behaviour: just an invocation_id. No query (so no
        // slice), default view = Original.
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
        assert!(matches!(parsed.query, Some(FetchQuery::Tail { lines: 10 })));
    }

    /// `tool_output_fetch`'s `description()` is now the only
    /// agent-facing advertisement of the call shape (the per-envelope
    /// `fetch_hint` was retired in favour of delegating to
    /// `describe_tool` / the catalog). Drift between the description
    /// and the schema is still a real bug — agents follow the
    /// description literally and hit `deny_unknown_fields` rejections.
    #[test]
    fn description_documents_view_and_each_query_mode() {
        assert!(DESCRIPTION.contains("view"), "description should mention `view`");
        assert!(DESCRIPTION.contains("query"), "description should mention `query`");
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
                DESCRIPTION.contains(&format!("`{mode}`")),
                "description must list mode `{mode}`",
            );
            let raw = serde_json::json!({
                "invocation_id": "00000000-0000-0000-0000-000000000000",
                "query": sample_args,
            });
            serde_json::from_value::<FetchInput>(raw)
                .unwrap_or_else(|e| panic!("description-shaped `{mode}` request must parse: {e}"));
        }
    }
}
