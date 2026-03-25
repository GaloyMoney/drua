use std::sync::Arc;

use style_agent_core::request_log::{RequestLogEntry, StatsResponse};
use style_agent_core::search::SearchEngine;

/// A row returned by `recent_requests`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestLogRow {
    pub id: i64,
    pub tool_name: String,
    pub query: String,
    pub num_results: i32,
    pub top_score: Option<f64>,
    pub latency_ms: i32,
    pub label_filter: Option<String>,
    pub language_filter: Option<String>,
    pub layer_filter: Option<String>,
    pub repo_filter: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

/// Trait for logging style-agent requests and querying metrics.
///
/// Implementations may write to SQLite (standalone) or Postgres (gateway).
#[async_trait::async_trait]
pub trait RequestLogger: Send + Sync + 'static {
    /// Record a completed request.
    async fn log_request(&self, entry: &RequestLogEntry) -> anyhow::Result<()>;

    /// Return aggregated stats (counts, rates, top queries).
    async fn query_stats(&self, low_score_threshold: f64) -> anyhow::Result<StatsResponse>;

    /// Return the most recent request log rows.
    async fn recent_requests(&self, limit: i64) -> anyhow::Result<Vec<RequestLogRow>>;
}

/// [`RequestLogger`] backed by the style-agent SQLite database via [`SearchEngine`].
#[derive(Clone)]
pub struct SqliteRequestLogger {
    engine: Arc<SearchEngine>,
}

impl SqliteRequestLogger {
    pub fn new(engine: Arc<SearchEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait::async_trait]
impl RequestLogger for SqliteRequestLogger {
    async fn log_request(&self, entry: &RequestLogEntry) -> anyhow::Result<()> {
        let engine = Arc::clone(&self.engine);
        let entry = RequestLogEntry {
            ts: entry.ts.clone(),
            tool: entry.tool.clone(),
            query: entry.query.clone(),
            filters: entry.filters.clone(),
            result_count: entry.result_count,
            top_score: entry.top_score,
            latency_ms: entry.latency_ms,
            error: entry.error.clone(),
        };
        tokio::task::spawn_blocking(move || engine.log_request(&entry)).await??;
        Ok(())
    }

    async fn query_stats(&self, low_score_threshold: f64) -> anyhow::Result<StatsResponse> {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || engine.query_stats(low_score_threshold)).await?
    }

    async fn recent_requests(&self, limit: i64) -> anyhow::Result<Vec<RequestLogRow>> {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || {
            // SQLite recent_requests is not implemented in the core store,
            // so we return an empty vec for standalone mode.
            let _ = limit;
            let _ = engine;
            Ok(vec![])
        })
        .await?
    }
}
