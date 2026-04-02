use pgvector::Vector;
use sqlx::PgPool;

use crate::primitives::ReportId;

use super::error::ReportError;
use super::StoreReportParams;

/// FTS keyword search result before merging.
#[derive(Debug)]
pub(super) struct FtsResult {
    pub(super) id: ReportId,
}

/// Vector similarity search result before merging.
#[derive(Debug)]
pub(super) struct VecResult {
    pub(super) id: ReportId,
}

/// PostgreSQL-backed search store for hybrid FTS + vector search.
///
/// Manages the `report_search_data` auxiliary table that sits alongside
/// the es-entity managed `reports` table.
#[derive(Clone)]
pub(super) struct SearchStore {
    pool: PgPool,
}

impl SearchStore {
    pub(super) fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    // ── Insert / Update ───────────────────────────────────────────────

    /// Insert search data for a newly created report.
    pub(super) async fn insert_search_data(
        &self,
        report_id: ReportId,
        params: &StoreReportParams,
    ) -> Result<(), ReportError> {
        let id: uuid::Uuid = report_id.into();
        let tags_json = serde_json::to_value(&params.tags)?;

        sqlx::query(
            "INSERT INTO report_search_data
                (report_id, title_text, content_text, tags)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(&params.title)
        .bind(&params.content)
        .bind(&tags_json)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Store a vector embedding for a report.
    pub(super) async fn upsert_embedding(
        &self,
        report_id: ReportId,
        embedding: &[f32],
    ) -> Result<(), ReportError> {
        let id: uuid::Uuid = report_id.into();
        let vector = Vector::from(embedding.to_vec());

        sqlx::query("UPDATE report_search_data SET embedding = $1 WHERE report_id = $2")
            .bind(vector)
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    // ── Query ─────────────────────────────────────────────────────────

    /// List report IDs ordered by creation time DESC.
    pub(super) async fn list_ids(&self, limit: i64) -> Result<Vec<ReportId>, ReportError> {
        let rows = sqlx::query_as::<_, IdRow>(
            "SELECT sd.report_id as id
             FROM report_search_data sd
             JOIN reports m ON m.id = sd.report_id
             ORDER BY m.created_at DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| ReportId::from(r.id)).collect())
    }

    /// Find a report ID by prefix match.
    pub(super) async fn find_id_by_prefix(
        &self,
        prefix: &str,
    ) -> Result<Option<ReportId>, ReportError> {
        let pattern = format!("{prefix}%");
        let row = sqlx::query_as::<_, IdRow>(
            "SELECT report_id as id FROM report_search_data
             WHERE report_id::text LIKE $1
             LIMIT 1",
        )
        .bind(&pattern)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ReportId::from(r.id)))
    }

    // ── Search ────────────────────────────────────────────────────────

    /// Full-text keyword search, returning ranked report IDs.
    pub(super) async fn search_fts(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<FtsResult>, ReportError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as::<_, IdRow>(
            "SELECT report_id as id
             FROM report_search_data
             WHERE search_tsv @@ plainto_tsquery('english', $1)
             ORDER BY ts_rank(search_tsv, plainto_tsquery('english', $1)) DESC
             LIMIT $2",
        )
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| FtsResult {
                id: ReportId::from(r.id),
            })
            .collect())
    }

    /// Vector similarity search (KNN), returning ranked report IDs.
    pub(super) async fn search_vector(
        &self,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<VecResult>, ReportError> {
        let vector = Vector::from(query_embedding.to_vec());

        let rows = sqlx::query_as::<_, IdRow>(
            "SELECT report_id as id
             FROM report_search_data
             WHERE embedding IS NOT NULL
             ORDER BY embedding <=> $1
             LIMIT $2",
        )
        .bind(vector)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| VecResult {
                id: ReportId::from(r.id),
            })
            .collect())
    }

    /// Check whether any embeddings exist.
    pub(super) async fn has_embeddings(&self) -> Result<bool, ReportError> {
        let row = sqlx::query_as::<_, CountRow>(
            "SELECT COUNT(*) as count FROM report_search_data WHERE embedding IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(row.count.unwrap_or(0) > 0)
    }
}

// ── Row types for sqlx ───────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct IdRow {
    id: uuid::Uuid,
}

#[derive(sqlx::FromRow)]
struct CountRow {
    count: Option<i64>,
}
