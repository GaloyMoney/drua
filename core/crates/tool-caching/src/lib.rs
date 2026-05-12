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

use rmcp::model::{CallToolResult, RawContent};

#[derive(Clone)]
pub struct ToolCaching {
    #[allow(dead_code)]
    pool: sqlx::PgPool,
    config: ToolCachingConfig,
    repo: ToolCacheRepo,
    walker: Walker,
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

    /// Passthrough early-returns:
    ///   * no owner (workflow executor / anonymous) — nothing to attribute
    ///   * upstream marked the result `is_error` — error responses flow through
    ///   * non-text content (image, multi-part) — only single-text-content
    ///     results are summarisable today
    pub async fn maybe_summarize_and_cache(
        &self,
        owner: impl Into<Option<ToolCallOwnerId>>,
        tool_name: &str,
        args: &serde_json::Value,
        result: CallToolResult,
    ) -> Result<ToolCacheResponse, ToolCachingError> {
        let Some(owner_id) = owner.into() else {
            return Ok(ToolCacheResponse {
                result,
                elided_paths: Vec::new(),
                invocation_id: None,
            });
        };
        if result.is_error == Some(true) || !is_simple_text_result(&result) {
            return Ok(ToolCacheResponse {
                result,
                elided_paths: Vec::new(),
                invocation_id: None,
            });
        }

        let original_structured = result.structured_content.clone();
        let original_text = extract_text(&result);

        let query_structure = QueryStructure::new(&original_text);
        // Mint the id up front so recover templates carry the real uuid;
        // persistence reuses the same value as the row's primary key.
        let invocation_id = ToolInvocationId::new();
        let summary = self
            .walker
            .summarize(&query_structure, invocation_id, tool_name);

        // Nothing was elided ⇒ upstream result is correct verbatim; skip
        // both persistence and the envelope rebuild so byte-for-byte
        // passthrough is preserved.
        if summary.elided_paths.is_empty() {
            return Ok(ToolCacheResponse {
                result,
                elided_paths: Vec::new(),
                invocation_id: None,
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

        let elided_paths = summary.elided_paths.clone();
        let wrapped = summary.into_call_tool_result(original_structured);
        Ok(ToolCacheResponse {
            result: wrapped,
            elided_paths,
            invocation_id: Some(invocation_id),
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
}

fn extract_text(result: &CallToolResult) -> String {
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
