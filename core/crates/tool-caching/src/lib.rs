mod config;
mod error;
mod fetch;
mod preprocessors;
mod primitives;
mod repo;
mod string_summarizer;
mod summarizer_passes;
mod walker;

pub use config::ToolCachingConfig;
pub use error::ToolCachingError;
pub use fetch::{FetchQuery, FetchResult};
pub use primitives::{
    ElidedPath, QueryStructure, ToolCacheResponse, ToolCallOwnerId, ToolCallSummary,
    ToolInvocationId,
};
pub use repo::StoredInvocation;
pub use string_summarizer::StringSummarizerChain;
pub use walker::Walker;

use repo::ToolCacheRepo;

use rmcp::model::{CallToolResult, Content, RawContent};
use serde_json::Value;

#[derive(Clone)]
pub struct ToolCaching {
    #[allow(dead_code)]
    pool: sqlx::PgPool,
    config: ToolCachingConfig,
    repo: ToolCacheRepo,
    walker: Walker,
}

/// Internal walk + persist result. Shared by the two public entry points.
struct Processed {
    summary: ToolCallSummary,
    invocation_id: ToolInvocationId,
    upstream_t: Value,
    persisted: bool,
}

impl ToolCaching {
    pub fn new(pool: &sqlx::PgPool, config: ToolCachingConfig) -> Self {
        let walker = Walker::new(
            std::sync::Arc::new(summarizer_passes::default_chain()),
            &config,
        );
        Self {
            pool: pool.clone(),
            config,
            repo: ToolCacheRepo::new(pool),
            walker,
        }
    }

    /// Agent-facing call path. Walks the upstream's structured channel
    /// (or parsed text as fallback), elides if over threshold, persists
    /// for `tool_output_fetch` recovery. Wire shape:
    ///
    /// * structured channel: `{result: T-elided, _elided?: {invocation_id, paths}}`
    /// * text channel: `<summary path="…" …>…</summary><recovery><elided …>…</elided></recovery>`
    ///
    /// If the upstream has no text content but does carry a structured
    /// channel, the text channel is filled with `serde_json::to_string_pretty`
    /// of the structured payload before walking — so callers (notably
    /// compose) can hand off an empty text channel and let cache() produce
    /// the agent-facing rendering.
    ///
    /// Passthrough early-returns (no persistence, raw CTR with the
    /// `{result: T}` wrapper added to structured for schema parity):
    ///   * no owner (workflow executor / anonymous)
    ///   * upstream marked the result `is_error`
    ///   * non-text content (image, multi-part)
    pub async fn cache(
        &self,
        owner: impl Into<Option<ToolCallOwnerId>>,
        tool_name: &str,
        args: &serde_json::Value,
        result: CallToolResult,
    ) -> Result<ToolCacheResponse, ToolCachingError> {
        let result = ensure_text_channel(result);
        let Some(owner_id) = owner.into() else {
            return Ok(passthrough_no_owner(result));
        };
        if result.is_error == Some(true) || !is_simple_text_result(&result) {
            return Ok(passthrough_no_owner(result));
        }

        let processed = self.process(owner_id, tool_name, args, &result).await?;

        if !processed.persisted {
            // Sub-threshold passthrough — keep the text channel raw,
            // still wrap structured as `{result: T}` so the wire shape
            // matches what `output_schema()` / `compose_types` advertise.
            let mut passthrough = result;
            passthrough.structured_content = Some(serde_json::json!({
                "result": processed.upstream_t,
            }));
            return Ok(ToolCacheResponse {
                result: passthrough,
                elided_paths: Vec::new(),
                invocation_id: None,
            });
        }

        let elided_paths = processed.summary.elided_paths.clone();
        let wrapped = build_elide_ctr(processed.summary, processed.invocation_id);
        Ok(ToolCacheResponse {
            result: wrapped,
            elided_paths,
            invocation_id: Some(processed.invocation_id),
        })
    }

    /// Compose sub-dispatch call path. The JS engine has its own size
    /// cap and scripts use the sub-tool's return directly as upstream `T`
    /// — pre-elided data would force every script to recover through
    /// `tool_output_fetch`. So the wire shape is `T verbatim` even when
    /// the value is over threshold.
    ///
    /// Walker still runs, summary is still persisted: the agent (out of
    /// the JS sandbox, reading `compose_response.sub_invocations[i].invocation_id`)
    /// can later call `tool_output_fetch(invocation_id, query={mode:"summary"})`
    /// to get the curated envelope they'd have seen from a top-level
    /// `call_tool` invocation.
    pub async fn persist_for_compose(
        &self,
        owner: impl Into<Option<ToolCallOwnerId>>,
        tool_name: &str,
        args: &serde_json::Value,
        result: CallToolResult,
    ) -> Result<ToolCacheResponse, ToolCachingError> {
        let result = ensure_text_channel(result);
        let Some(owner_id) = owner.into() else {
            return Ok(passthrough_no_owner(result));
        };
        if result.is_error == Some(true) || !is_simple_text_result(&result) {
            return Ok(passthrough_no_owner(result));
        }

        let processed = self.process(owner_id, tool_name, args, &result).await?;

        // For compose JS engine: always emit T verbatim, regardless of
        // whether the walker found anything elision-worthy.
        let mut wrapped = result;
        wrapped.structured_content = Some(processed.upstream_t);

        if !processed.persisted {
            return Ok(ToolCacheResponse {
                result: wrapped,
                elided_paths: Vec::new(),
                invocation_id: None,
            });
        }

        Ok(ToolCacheResponse {
            result: wrapped,
            elided_paths: processed.summary.elided_paths,
            invocation_id: Some(processed.invocation_id),
        })
    }

    /// Resolve a path on a previously-persisted invocation and slice
    /// the resolved value with `query`. Owner-scoped — non-owners get
    /// `InvocationNotFound`. Wraps the result back at `path` so the
    /// response mirrors the caller's request shape. Responses larger
    /// than `config.max_fetch_response_bytes` are rejected with
    /// `FetchResponseTooLarge`.
    pub async fn fetch(
        &self,
        owner_id: ToolCallOwnerId,
        id: ToolInvocationId,
        path: &str,
        query: Option<&FetchQuery>,
    ) -> Result<FetchResult, ToolCachingError> {
        let stored = self.repo.find_by_id(id, owner_id).await?;
        stored.query(path, query, self.config.max_fetch_response_bytes)
    }

    /// Shared walk + persist. Both public entry points feed into this:
    /// `cache()` consumes `processed.summary` to build the elided wire
    /// shape; `persist_for_compose()` discards the summary on the wire
    /// (emits T verbatim) but persists it so the agent can re-fetch
    /// with `{mode: "summary"}` later.
    async fn process(
        &self,
        owner_id: ToolCallOwnerId,
        tool_name: &str,
        args: &serde_json::Value,
        result: &CallToolResult,
    ) -> Result<Processed, ToolCachingError> {
        let original_structured = result.structured_content.clone();
        let original_text = extract_text(result);

        // Walk the upstream's structured channel when present — that's
        // the canonical `T` whose shape MCP clients validate against the
        // advertised outputSchema. Fall back to parsed-text Value for
        // text-only tools (bash, k8s logs, etc.).
        let query_structure = QueryStructure {
            root: original_structured
                .clone()
                .unwrap_or_else(|| QueryStructure::new(&original_text).root),
        };
        let invocation_id = ToolInvocationId::new();
        let summary = self
            .walker
            .summarize(&query_structure, invocation_id, tool_name);

        let upstream_t = original_structured
            .clone()
            .unwrap_or_else(|| query_structure.root.clone());

        if summary.elided_paths.is_empty() {
            return Ok(Processed {
                summary,
                invocation_id,
                upstream_t,
                persisted: false,
            });
        }

        self.repo
            .persist(
                invocation_id,
                owner_id,
                tool_name,
                args,
                &query_structure,
                &summary,
                &original_text,
                original_structured.as_ref(),
            )
            .await?;

        Ok(Processed {
            summary,
            invocation_id,
            upstream_t,
            persisted: true,
        })
    }
}

/// Owner-less passthrough — workflow executor / anonymous callers
/// don't get persistence or wrapping.
fn passthrough_no_owner(result: CallToolResult) -> ToolCacheResponse {
    ToolCacheResponse {
        result,
        elided_paths: Vec::new(),
        invocation_id: None,
    }
}

/// Build the agent-facing wrapped `CallToolResult` — delegates to
/// `ToolCallSummary::build_wire` so this and the `FetchQuery::Summary`
/// replay path share one wire-shape construction site.
fn build_elide_ctr(summary: ToolCallSummary, invocation_id: ToolInvocationId) -> CallToolResult {
    summary.build_wire(invocation_id).0
}

pub fn extract_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_simple_text_result(result: &CallToolResult) -> bool {
    result.content.len() == 1 && matches!(result.content[0].raw, RawContent::Text(_))
}

/// If the upstream carries a structured channel and the content vec is
/// empty, fill the text channel with a pretty-printed JSON rendering of
/// the structured payload. Lets callers (notably compose) pass an empty
/// content vec and rely on `cache()` to produce the agent-facing
/// rendering — either as the passthrough text or, when walking elides
/// something, replaced in place by the `<summary>+<recovery>` envelope.
///
/// Strictly `is_empty()`, not "no text content present": multi-part
/// results (e.g. images alongside structured data) must keep their
/// non-text parts intact and fall through to the
/// `!is_simple_text_result` passthrough below.
fn ensure_text_channel(mut result: CallToolResult) -> CallToolResult {
    if !result.content.is_empty() {
        return result;
    }
    let Some(structured) = result.structured_content.as_ref() else {
        return result;
    };
    let text = serde_json::to_string_pretty(structured).unwrap_or_default();
    result.content = vec![Content::text(text)];
    result
}
