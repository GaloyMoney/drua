pub mod error;

use style_agent_server::request_log::{LabelCount, QueryCount, RequestLogEntry, StatsResponse};
use style_agent_server::{LoggerError, RequestLogRow, RequestLogger};
use tracing::instrument;

pub use error::StyleAgentLogsError;

/// A single request log row for display in the web UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StyleAgentRequestRow {
    pub id: i64,
    pub tool_name: String,
    pub query: String,
    pub num_results: i32,
    pub top_score: Option<f64>,
    pub top_score_fmt: String,
    pub latency_ms: i32,
    pub label_filter: Option<String>,
    pub language_filter: Option<String>,
    pub repo_filter: Option<String>,
    pub error: Option<String>,
    pub has_error: bool,
    pub created_at: String,
}

/// A query string and how often it was used (for the dashboard).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopQuery {
    pub query: String,
    pub count: i64,
}

/// Aggregated dashboard stats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DashboardStats {
    pub total_requests_24h: i64,
    pub total_requests_7d: i64,
    pub avg_latency_ms_fmt: String,
    pub error_rate_fmt: String,
    pub low_score_rate_fmt: String,
    pub top_queries: Vec<TopQuery>,
}

/// Raw row type from the `style_agent_request_log` table.
type RawRequestRow = (
    i64,
    String,
    String,
    i32,
    Option<f64>,
    i32,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
);

/// Service for writing and reading style-agent request logs in Postgres.
///
/// Implements [`RequestLogger`] (write path from the MCP gateway) and
/// exposes dashboard-specific read queries.
#[derive(Clone)]
pub struct StyleAgentLogs {
    pool: sqlx::PgPool,
}

impl StyleAgentLogs {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Return recent request log rows (most recent first) for the web dashboard.
    #[instrument(name = "domain.style_agent_logs.recent_requests", skip_all)]
    pub async fn recent_requests(
        &self,
        limit: i64,
    ) -> Result<Vec<StyleAgentRequestRow>, StyleAgentLogsError> {
        let rows: Vec<RawRequestRow> = sqlx::query_as(
            r#"SELECT
                id, tool_name, query, num_results, top_score, latency_ms,
                label_filter, language_filter, repo_filter, error,
                created_at
            FROM style_agent_request_log
            ORDER BY created_at DESC
            LIMIT $1"#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let top_score_fmt =
                    r.4.map(|s| format!("{s:.3}"))
                        .unwrap_or_else(|| "—".to_string());
                let has_error = r.9.is_some();
                StyleAgentRequestRow {
                    id: r.0,
                    tool_name: r.1,
                    query: r.2,
                    num_results: r.3,
                    top_score: r.4,
                    top_score_fmt,
                    latency_ms: r.5,
                    label_filter: r.6,
                    language_filter: r.7,
                    repo_filter: r.8,
                    error: r.9,
                    has_error,
                    created_at: r.10.format("%Y-%m-%d %H:%M:%S").to_string(),
                }
            })
            .collect())
    }

    /// Return aggregated dashboard stats for the web UI.
    #[instrument(name = "domain.style_agent_logs.dashboard_stats", skip_all)]
    pub async fn dashboard_stats(&self) -> Result<DashboardStats, StyleAgentLogsError> {
        let low_score_threshold = 0.3_f64;

        let row: (i64, i64, i64, i64, i64, f64) = sqlx::query_as(
            r#"SELECT
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '1 day'),
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '7 days'),
                COUNT(*) FILTER (WHERE top_score IS NOT NULL AND top_score < $1 AND created_at > NOW() - INTERVAL '7 days'),
                COUNT(*) FILTER (WHERE error IS NOT NULL AND created_at > NOW() - INTERVAL '7 days'),
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '7 days'),
                COALESCE(AVG(latency_ms::double precision) FILTER (WHERE created_at > NOW() - INTERVAL '7 days'), 0)
            FROM style_agent_request_log"#,
        )
        .bind(low_score_threshold)
        .fetch_one(&self.pool)
        .await?;

        let (cnt_24h, cnt_7d, low_score_cnt, error_cnt, total_7d, avg_latency) = row;
        let total = total_7d.max(1) as f64;

        let top_queries: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT query, COUNT(*) AS cnt
            FROM style_agent_request_log
            WHERE created_at > NOW() - INTERVAL '7 days'
            GROUP BY query
            ORDER BY cnt DESC
            LIMIT 10"#,
        )
        .fetch_all(&self.pool)
        .await?;

        let error_rate = error_cnt as f64 / total * 100.0;
        let low_score_rate = low_score_cnt as f64 / total * 100.0;

        Ok(DashboardStats {
            total_requests_24h: cnt_24h,
            total_requests_7d: cnt_7d,
            avg_latency_ms_fmt: format!("{avg_latency:.0}"),
            error_rate_fmt: format!("{error_rate:.1}%"),
            low_score_rate_fmt: format!("{low_score_rate:.1}%"),
            top_queries: top_queries
                .into_iter()
                .map(|(query, count)| TopQuery { query, count })
                .collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// RequestLogger implementation — write path (fire-and-forget from MCP gateway)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl RequestLogger for StyleAgentLogs {
    #[instrument(name = "domain.style_agent_logs.log_request", skip_all)]
    async fn log_request(&self, entry: &RequestLogEntry) -> Result<(), LoggerError> {
        let filters: Option<serde_json::Value> = entry
            .filters
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());

        let label_filter = filters
            .as_ref()
            .and_then(|f| f.get("label"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let language_filter = filters
            .as_ref()
            .and_then(|f| f.get("language"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let layer_filter = filters
            .as_ref()
            .and_then(|f| f.get("layer"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let repo_filter = filters
            .as_ref()
            .and_then(|f| f.get("repo"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let num_results = entry.result_count as i32;
        let latency_ms = entry.latency_ms as i32;
        let results_jsonb: Option<serde_json::Value> = entry
            .results_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());

        sqlx::query(
            r#"INSERT INTO style_agent_request_log
                (tool_name, query, num_results, top_score, latency_ms,
                 label_filter, language_filter, layer_filter, repo_filter, error, results)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
        )
        .bind(&entry.tool)
        .bind(&entry.query)
        .bind(num_results)
        .bind(entry.top_score)
        .bind(latency_ms)
        .bind(&label_filter)
        .bind(&language_filter)
        .bind(&layer_filter)
        .bind(&repo_filter)
        .bind(&entry.error)
        .bind(&results_jsonb)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[instrument(name = "domain.style_agent_logs.query_stats", skip_all)]
    async fn query_stats(&self, low_score_threshold: f64) -> Result<StatsResponse, LoggerError> {
        let row: (i64, i64, i64, i64, i64, i64, f64) = sqlx::query_as(
            r#"SELECT
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '1 day'),
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '7 days'),
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '30 days'),
                COUNT(*) FILTER (WHERE num_results = 0 AND created_at > NOW() - INTERVAL '30 days'),
                COUNT(*) FILTER (WHERE top_score IS NOT NULL AND top_score < $1 AND created_at > NOW() - INTERVAL '30 days'),
                COUNT(*) FILTER (WHERE error IS NOT NULL AND created_at > NOW() - INTERVAL '30 days'),
                COALESCE(AVG(latency_ms::double precision) FILTER (WHERE created_at > NOW() - INTERVAL '30 days'), 0)
            FROM style_agent_request_log"#,
        )
        .bind(low_score_threshold)
        .fetch_one(&self.pool)
        .await?;

        let (cnt_24h, cnt_7d, cnt_30d, empty_cnt, low_score_cnt, error_cnt, avg_latency) = row;
        let total_30d = cnt_30d.max(1) as f64;

        let top_queries: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT query, COUNT(*) AS cnt
            FROM style_agent_request_log
            WHERE created_at > NOW() - INTERVAL '30 days'
            GROUP BY query
            ORDER BY cnt DESC
            LIMIT 20"#,
        )
        .fetch_all(&self.pool)
        .await?;

        let top_labels: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT label_filter, COUNT(*) AS cnt
            FROM style_agent_request_log
            WHERE created_at > NOW() - INTERVAL '30 days' AND label_filter IS NOT NULL
            GROUP BY label_filter
            ORDER BY cnt DESC
            LIMIT 20"#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(StatsResponse {
            total_requests_24h: cnt_24h,
            total_requests_7d: cnt_7d,
            total_requests_30d: cnt_30d,
            empty_result_rate: empty_cnt as f64 / total_30d,
            low_score_rate: low_score_cnt as f64 / total_30d,
            low_score_threshold,
            error_rate: error_cnt as f64 / total_30d,
            avg_latency_ms: avg_latency,
            top_queries: top_queries
                .into_iter()
                .map(|(query, count)| QueryCount { query, count })
                .collect(),
            top_labels: top_labels
                .into_iter()
                .map(|(label, count)| LabelCount { label, count })
                .collect(),
        })
    }

    #[instrument(name = "domain.style_agent_logs.recent_requests_log", skip_all)]
    async fn recent_requests(&self, limit: i64) -> Result<Vec<RequestLogRow>, LoggerError> {
        let rows: Vec<RawRequestRow> = sqlx::query_as(
            r#"SELECT
                id, tool_name, query, num_results, top_score, latency_ms,
                label_filter, language_filter, repo_filter, error,
                created_at
            FROM style_agent_request_log
            ORDER BY created_at DESC
            LIMIT $1"#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| RequestLogRow {
                id: r.0,
                tool_name: r.1,
                query: r.2,
                num_results: r.3,
                top_score: r.4,
                latency_ms: r.5,
                label_filter: r.6,
                language_filter: r.7,
                layer_filter: None,
                repo_filter: r.8,
                error: r.9,
                created_at: r.10.format("%Y-%m-%d %H:%M:%S").to_string(),
            })
            .collect())
    }
}
