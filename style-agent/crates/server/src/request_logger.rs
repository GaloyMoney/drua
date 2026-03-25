use style_agent_core::request_log::{RequestLogEntry, StatsResponse};

/// Boxed error type used by [`RequestLogger`] trait methods.
pub type LoggerError = Box<dyn std::error::Error + Send + Sync>;

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
/// Implementations may write to Postgres (gateway) or silently discard (standalone).
#[async_trait::async_trait]
pub trait RequestLogger: Send + Sync + 'static {
    /// Record a completed request.
    async fn log_request(&self, entry: &RequestLogEntry) -> Result<(), LoggerError>;

    /// Return aggregated stats (counts, rates, top queries).
    async fn query_stats(&self, low_score_threshold: f64) -> Result<StatsResponse, LoggerError>;

    /// Return the most recent request log rows.
    async fn recent_requests(&self, limit: i64) -> Result<Vec<RequestLogRow>, LoggerError>;
}

/// No-op [`RequestLogger`] that silently discards all entries.
///
/// Used as the default when no external logger (e.g. Postgres) is configured.
#[derive(Clone)]
pub struct NoopRequestLogger;

#[async_trait::async_trait]
impl RequestLogger for NoopRequestLogger {
    async fn log_request(&self, _entry: &RequestLogEntry) -> Result<(), LoggerError> {
        Ok(())
    }

    async fn query_stats(&self, low_score_threshold: f64) -> Result<StatsResponse, LoggerError> {
        Ok(StatsResponse {
            total_requests_24h: 0,
            total_requests_7d: 0,
            total_requests_30d: 0,
            empty_result_rate: 0.0,
            low_score_rate: 0.0,
            low_score_threshold,
            error_rate: 0.0,
            avg_latency_ms: 0.0,
            top_queries: vec![],
            top_labels: vec![],
        })
    }

    async fn recent_requests(&self, _limit: i64) -> Result<Vec<RequestLogRow>, LoggerError> {
        Ok(vec![])
    }
}
