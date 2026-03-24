use serde::Serialize;

/// Entry to be inserted into the request_log table.
pub struct RequestLogEntry {
    pub ts: String,
    pub tool: String,
    pub query: String,
    pub filters: Option<String>,
    pub result_count: i64,
    pub top_score: Option<f64>,
    pub latency_ms: i64,
    pub error: Option<String>,
}

/// Aggregated stats returned by `GET /stats`.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_requests_24h: i64,
    pub total_requests_7d: i64,
    pub total_requests_30d: i64,
    pub empty_result_rate: f64,
    pub low_score_rate: f64,
    pub low_score_threshold: f64,
    pub error_rate: f64,
    pub avg_latency_ms: f64,
    pub top_queries: Vec<QueryCount>,
    pub top_labels: Vec<LabelCount>,
}

/// A query string and how often it appeared.
#[derive(Debug, Serialize)]
pub struct QueryCount {
    pub query: String,
    pub count: i64,
}

/// A label filter value and how often it appeared.
#[derive(Debug, Serialize)]
pub struct LabelCount {
    pub label: String,
    pub count: i64,
}
