pub mod error;

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

/// A query string and how often it was used.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryCount {
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
    pub top_queries: Vec<QueryCount>,
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

/// Read-only service for querying style-agent request logs from Postgres.
#[derive(Clone)]
pub struct StyleAgentLogs {
    pool: sqlx::PgPool,
}

impl StyleAgentLogs {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Return recent request log rows (most recent first).
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

    /// Return aggregated dashboard stats.
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
                .map(|(query, count)| QueryCount { query, count })
                .collect(),
        })
    }
}
