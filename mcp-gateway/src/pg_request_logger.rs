use style_agent_core::request_log::{LabelCount, QueryCount, RequestLogEntry, StatsResponse};
use style_agent_server::{RequestLogRow, RequestLogger};
use tracing::instrument;

/// [`RequestLogger`] backed by PostgreSQL via [`sqlx::PgPool`].
#[derive(Clone)]
pub struct PgRequestLogger {
    pool: sqlx::PgPool,
}

impl PgRequestLogger {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}

#[async_trait::async_trait]
impl RequestLogger for PgRequestLogger {
    #[instrument(name = "mcp_gateway.pg_request_logger.log_request", skip_all)]
    async fn log_request(&self, entry: &RequestLogEntry) -> anyhow::Result<()> {
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

        sqlx::query(
            r#"INSERT INTO style_agent_request_log
                (tool_name, query, num_results, top_score, latency_ms,
                 label_filter, language_filter, layer_filter, repo_filter, error)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
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
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[instrument(name = "mcp_gateway.pg_request_logger.query_stats", skip_all)]
    async fn query_stats(&self, low_score_threshold: f64) -> anyhow::Result<StatsResponse> {
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

    #[instrument(name = "mcp_gateway.pg_request_logger.recent_requests", skip_all)]
    async fn recent_requests(&self, limit: i64) -> anyhow::Result<Vec<RequestLogRow>> {
        let rows: Vec<(
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
            Option<String>,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            r#"SELECT
                id, tool_name, query, num_results, top_score, latency_ms,
                label_filter, language_filter, layer_filter, repo_filter, error,
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
                layer_filter: r.8,
                repo_filter: r.9,
                error: r.10,
                created_at: r.11.format("%Y-%m-%d %H:%M:%S").to_string(),
            })
            .collect())
    }
}
