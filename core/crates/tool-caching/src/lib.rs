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
pub use fetch::{ensure_object, fetch_text_for_raw, FetchQuery, FetchResult};
pub use primitives::{
    ElidedPath, QueryStructure, SummaryFetchInfo, ToolCacheResponse, ToolCallOwnerId,
    ToolCallSummary, ToolInvocationId,
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
        if result.is_error == Some(true) {
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
                summary_fetch_info: None,
                root_path: None,
            });
        }

        let elided_paths = processed.summary.elided_paths.clone();
        let summary_fetch_info = self.summary_fetch_info(&processed.summary);
        let root_path = processed.summary.root_path.clone();
        let wrapped = build_elide_ctr(processed.summary, processed.invocation_id);
        Ok(ToolCacheResponse {
            result: wrapped,
            elided_paths,
            invocation_id: Some(processed.invocation_id),
            summary_fetch_info: Some(summary_fetch_info),
            root_path: Some(root_path),
        })
    }

    /// Variant of [`cache`] for tools that own their envelope shape and opt
    /// out of the `DruaToolResult` wrapper (`default_tool_caching() == false`,
    /// e.g. `compose`). Walks and persists identically to `cache()`, but on
    /// the structured channel emits the upstream `T` verbatim — merging
    /// elision recovery (`_recovery` / `_elided`) into `T`'s top-level
    /// object when the walker elides, rather than nesting under a
    /// synthetic `result` key.
    ///
    /// This is what makes the returned `structuredContent` validate against
    /// such a tool's advertised `outputSchema` (e.g. `ComposeOutput`): the
    /// schema has no outer `{ result: ... }` wrapper, so `cache()`'s wrap
    /// would make every persisted compose call fail strict MCP-client
    /// output-schema validation (`@modelcontextprotocol/sdk` `callTool`
    /// validates unconditionally when a tool declares an `outputSchema`).
    pub async fn cache_envelope(
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
        if result.is_error == Some(true) {
            return Ok(passthrough_no_owner(result));
        }

        let processed = self.process(owner_id, tool_name, args, &result).await?;

        if !processed.persisted {
            // Sub-threshold passthrough — emit T verbatim, no `{result: T}`
            // wrap. Matches what envelope-owning tools advertise as their
            // outputSchema.
            let mut passthrough = result;
            passthrough.structured_content = Some(processed.upstream_t);
            return Ok(ToolCacheResponse {
                result: passthrough,
                elided_paths: Vec::new(),
                invocation_id: None,
                summary_fetch_info: None,
                root_path: None,
            });
        }

        let elided_paths = processed.summary.elided_paths.clone();
        let summary_fetch_info = self.summary_fetch_info(&processed.summary);
        let root_path = processed.summary.root_path.clone();
        let wrapped = build_elide_ctr_envelope(processed.summary, processed.invocation_id);
        Ok(ToolCacheResponse {
            result: wrapped,
            elided_paths,
            invocation_id: Some(processed.invocation_id),
            summary_fetch_info: Some(summary_fetch_info),
            root_path: Some(root_path),
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
        if result.is_error == Some(true) {
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
                summary_fetch_info: None,
                root_path: None,
            });
        }

        let summary_fetch_info = self.summary_fetch_info(&processed.summary);
        let root_path = processed.summary.root_path.clone();
        Ok(ToolCacheResponse {
            result: wrapped,
            elided_paths: processed.summary.elided_paths,
            invocation_id: Some(processed.invocation_id),
            summary_fetch_info: Some(summary_fetch_info),
            root_path: Some(root_path),
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

    pub fn max_fetch_response_bytes(&self) -> u64 {
        self.config.max_fetch_response_bytes as u64
    }

    fn summary_fetch_info(&self, summary: &ToolCallSummary) -> SummaryFetchInfo {
        let summary_envelope_bytes = summary.build_envelope_text().len() as u64;
        let normal_fetch_limit_bytes = self.max_fetch_response_bytes();
        SummaryFetchInfo {
            summary_envelope_bytes,
            normal_fetch_limit_bytes,
        }
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
        let upstream_t = tool_result_value(result);
        let original_text = if is_simple_text_result(result) {
            extract_text(result)
        } else {
            serde_json::to_string_pretty(&upstream_t).unwrap_or_else(|_| extract_text(result))
        };

        // Walk the upstream's structured channel when present — that's
        // the canonical `T` whose shape MCP clients validate against the
        // advertised outputSchema. Fall back to a normalized content value
        // for MCP content-only tools, including multi-part text/resource
        // responses.
        let query_structure = QueryStructure {
            root: upstream_t.clone(),
        };
        let invocation_id = ToolInvocationId::new();
        let summary = self
            .walker
            .summarize(&query_structure, invocation_id, tool_name);

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
        summary_fetch_info: None,
        root_path: None,
    }
}

/// Build the agent-facing wrapped `CallToolResult` — delegates to
/// `ToolCallSummary::build_wire` so this and the `FetchQuery::Summary`
/// replay path share one wire-shape construction site.
fn build_elide_ctr(summary: ToolCallSummary, invocation_id: ToolInvocationId) -> CallToolResult {
    summary.build_wire(invocation_id).0
}

/// Envelope-owning variant (see [`ToolCallSummary::build_wire_envelope`]).
/// Used by [`ToolCaching::cache_envelope`] for tools like `compose` whose
/// advertised outputSchema has no outer `{ result: ... }` wrapper.
fn build_elide_ctr_envelope(
    summary: ToolCallSummary,
    invocation_id: ToolInvocationId,
) -> CallToolResult {
    summary.build_wire_envelope(invocation_id).0
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

/// Canonical JSON view of a tool result used by the recovery walker and
/// compose. Prefer upstream structured content when present; otherwise parse a
/// single text block as before, or preserve multi-part MCP content as an
/// inspectable JSON object.
pub fn tool_result_value(result: &CallToolResult) -> Value {
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    if is_simple_text_result(result) {
        return QueryStructure::new(&extract_text(result)).root;
    }
    Value::Object(
        [(
            "content".to_string(),
            Value::Array(result.content.iter().map(content_to_value).collect()),
        )]
        .into_iter()
        .collect(),
    )
}

fn content_to_value(content: &Content) -> Value {
    let mut value = serde_json::to_value(content).unwrap_or(Value::Null);
    match &content.raw {
        RawContent::Image(image) => {
            replace_key(
                &mut value,
                "data",
                Value::String(format!("<base64 image elided; {} bytes>", image.data.len())),
            );
        }
        RawContent::Audio(audio) => {
            replace_key(
                &mut value,
                "data",
                Value::String(format!("<base64 audio elided; {} bytes>", audio.data.len())),
            );
        }
        RawContent::Resource(resource) => {
            if let rmcp::model::ResourceContents::BlobResourceContents { blob, .. } =
                &resource.resource
            {
                replace_key(
                    &mut value,
                    "blob",
                    Value::String(format!(
                        "<base64 resource blob elided; {} bytes>",
                        blob.len()
                    )),
                );
            }
        }
        RawContent::Text(_) | RawContent::ResourceLink(_) => {}
    }
    value
}

fn replace_key(value: &mut Value, key: &str, replacement: Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.contains_key(key) {
                map.insert(key.to_string(), replacement);
                true
            } else {
                map.values_mut()
                    .any(|child| replace_key(child, key, replacement.clone()))
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .any(|child| replace_key(child, key, replacement.clone())),
        _ => false,
    }
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
/// results keep their original content channel while the cache walker
/// operates on `tool_result_value`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_value_preserves_embedded_text_resources() {
        let result = CallToolResult::success(vec![
            Content::text("downloaded"),
            Content::resource(rmcp::model::ResourceContents::text(
                "file-body",
                "repo://owner/repo/file.txt",
            )),
        ]);

        let value = tool_result_value(&result);

        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "downloaded");
        assert_eq!(value["content"][1]["type"], "resource");
        assert_eq!(
            value["content"][1]["resource"]["uri"],
            "repo://owner/repo/file.txt"
        );
        assert_eq!(value["content"][1]["resource"]["text"], "file-body");
    }

    #[test]
    fn tool_result_value_omits_binary_payloads() {
        let result = CallToolResult::success(vec![
            Content::image("abcdefgh", "image/png"),
            Content::resource(rmcp::model::ResourceContents::blob(
                "0123456789",
                "repo://owner/repo/image.png",
            )),
        ]);

        let value = tool_result_value(&result);

        assert_eq!(
            value["content"][0]["data"],
            "<base64 image elided; 8 bytes>"
        );
        assert_eq!(
            value["content"][1]["resource"]["blob"],
            "<base64 resource blob elided; 10 bytes>"
        );
    }

    /// `build_wire_envelope` keeps the walked object's own top-level shape
    /// (no synthetic `result` wrapper) while still attaching `_recovery` /
    /// `_elided`. This is what lets envelope-owning tools like `compose`
    /// validate their `structuredContent` against a flat `outputSchema`
    /// (e.g. `ComposeOutput`). Mirrored against `build_wire` to lock the
    /// wrap-vs-merge contract.
    #[test]
    fn build_wire_envelope_merges_recovery_into_object_root() {
        use crate::primitives::{ElidedPath, ToolCallSummary};
        // `wire_result` mirrors a ComposeOutput-style envelope: a handful
        // of always-present fields plus a big `result` the walker elided.
        let wire_result = serde_json::json!({
            "sub_invocations": [],
            "tool_calls": 1,
            "execution_time_ms": 12,
            "console": [],
            "result": "<elided body>"
        });
        let elided = ElidedPath {
            path: "$.result".to_string(),
            total_bytes: 9999,
            shown_bytes: 16,
            total_lines: None,
            shown_lines: None,
            total_items: None,
            shown_items: None,
            recover: serde_json::json!({
                "tool": "tool_output_fetch",
                "args_template": { "invocation_id": "<id>", "path": "$.result" }
            }),
        };
        let summary = ToolCallSummary {
            summary: serde_json::json!("<elided body>"),
            wire_result: wire_result.clone(),
            elided_paths: vec![elided],
            root_path: "$".to_string(),
            total_bytes: 9999,
            shown_bytes: 16,
            total_items: None,
            shown_items: None,
            total_lines: None,
            shown_lines: None,
        };
        let id = ToolInvocationId::new();

        let (wrapped_ctr, wrapped) = summary.clone().build_wire(id);
        let (env_ctr, env) = summary.build_wire_envelope(id);

        // `build_wire` nests everything under `result` (DruaToolResult).
        assert_eq!(wrapped["result"]["tool_calls"], 1);
        assert!(wrapped.get("_recovery").is_some());
        assert!(wrapped.get("_elided").is_some());
        // The envelope's own fields are NOT at the wrapped root.
        assert!(wrapped.get("tool_calls").is_none());

        // `build_wire_envelope` keeps ComposeOutput's fields at the root.
        assert_eq!(env["tool_calls"], 1);
        assert_eq!(env["execution_time_ms"], 12);
        assert_eq!(env["sub_invocations"], serde_json::json!([]));
        assert_eq!(env["result"], "<elided body>");
        // Recovery metadata merged in alongside them, not under `result`.
        assert_eq!(
            env["_recovery"]["invocation_id"],
            wrapped["_recovery"]["invocation_id"]
        );
        assert_eq!(env["_elided"]["paths"][0]["path"], "$.result");
        // No synthetic `result` wrapper around the whole envelope.
        assert!(env.get("result").is_some()); // ComposeOutput.result, present
        assert!(env.get("_recovery").is_some());
        assert!(env.get("_elided").is_some());

        // Same text-channel envelope on both (recovery rendering is shared).
        assert_eq!(extract_text(&wrapped_ctr), extract_text(&env_ctr));
    }

    /// When `wire_result` is not an object (array/scalar root),
    /// `build_wire_envelope` must fall back to `build_wire` so recovery
    /// metadata is never silently dropped.
    #[test]
    fn build_wire_envelope_falls_back_to_wrap_for_non_object_root() {
        use crate::primitives::{ElidedPath, ToolCallSummary};
        let summary = ToolCallSummary {
            summary: serde_json::json!("x"),
            wire_result: serde_json::json!([1, 2, 3]), // array root
            elided_paths: vec![ElidedPath {
                path: "$[0]".to_string(),
                total_bytes: 10,
                shown_bytes: 1,
                total_lines: None,
                shown_lines: None,
                total_items: None,
                shown_items: None,
                recover: serde_json::json!({"tool": "tool_output_fetch"}),
            }],
            root_path: "$".to_string(),
            total_bytes: 10,
            shown_bytes: 1,
            total_items: None,
            shown_items: None,
            total_lines: None,
            shown_lines: None,
        };
        let id = ToolInvocationId::new();
        let (_, env) = summary.clone().build_wire_envelope(id);
        let (_, wrapped) = summary.build_wire(id);
        // Non-object root → identical wrapped shape (recovery preserved).
        assert_eq!(env, wrapped);
        assert_eq!(env["result"], serde_json::json!([1, 2, 3]));
        assert!(env.get("_recovery").is_some());
    }
}
