use pgvector::Vector;
use sqlx::PgPool;

use crate::primitives::MemoryId;

use super::error::MemoryError;
use super::StoreMemoryParams;

/// FTS keyword search result before merging.
#[derive(Debug)]
pub(super) struct FtsResult {
    pub(super) id: MemoryId,
}

/// Vector similarity search result before merging.
#[derive(Debug)]
pub(super) struct VecResult {
    pub(super) id: MemoryId,
}

/// PostgreSQL-backed search store for hybrid FTS + vector search.
///
/// Manages the `memory_search_data` auxiliary table that sits alongside
/// the es-entity managed `memories` table.
#[derive(Clone)]
pub(super) struct SearchStore {
    pool: PgPool,
}

impl SearchStore {
    pub(super) fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    // ── Insert / Update ───────────────────────────────────────────────

    /// Insert search data for a newly created memory.
    pub(super) async fn insert_search_data(
        &self,
        memory_id: MemoryId,
        params: &StoreMemoryParams,
    ) -> Result<(), MemoryError> {
        let id: uuid::Uuid = memory_id.into();
        let tags_json = serde_json::to_value(&params.tags)?;

        sqlx::query(
            "INSERT INTO memory_search_data
                (memory_id, title_text, content_text, tags)
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

    /// Store a vector embedding for a memory.
    pub(super) async fn upsert_embedding(
        &self,
        memory_id: MemoryId,
        embedding: &[f32],
    ) -> Result<(), MemoryError> {
        let id: uuid::Uuid = memory_id.into();
        let vector = Vector::from(embedding.to_vec());

        sqlx::query("UPDATE memory_search_data SET embedding = $1 WHERE memory_id = $2")
            .bind(vector)
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    // ── Query ─────────────────────────────────────────────────────────

    /// List memory IDs ordered by creation time DESC.
    pub(super) async fn list_ids(&self, limit: i64) -> Result<Vec<MemoryId>, MemoryError> {
        let rows = sqlx::query_as::<_, IdRow>(
            "SELECT sd.memory_id as id
             FROM memory_search_data sd
             JOIN memories m ON m.id = sd.memory_id
             ORDER BY m.created_at DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| MemoryId::from(r.id)).collect())
    }

    /// Find a memory ID by prefix match.
    pub(super) async fn find_id_by_prefix(
        &self,
        prefix: &str,
    ) -> Result<Option<MemoryId>, MemoryError> {
        let pattern = format!("{prefix}%");
        let row = sqlx::query_as::<_, IdRow>(
            "SELECT memory_id as id FROM memory_search_data
             WHERE memory_id::text LIKE $1
             LIMIT 1",
        )
        .bind(&pattern)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| MemoryId::from(r.id)))
    }

    /// Record access for a batch of memory IDs.
    pub(super) async fn record_access(&self, ids: &[MemoryId]) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }

        let uuids: Vec<uuid::Uuid> = ids.iter().map(|id| (*id).into()).collect();

        sqlx::query(
            "UPDATE memory_search_data
             SET last_accessed = NOW(), access_count = access_count + 1
             WHERE memory_id = ANY($1)",
        )
        .bind(&uuids)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get access metadata for a memory.
    pub(super) async fn get_access_metadata(
        &self,
        memory_id: MemoryId,
    ) -> Result<AccessMetadata, MemoryError> {
        let id: uuid::Uuid = memory_id.into();
        let row = sqlx::query_as::<_, AccessRow>(
            "SELECT last_accessed
             FROM memory_search_data WHERE memory_id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(AccessRow {
            last_accessed: None,
        });

        Ok(AccessMetadata {
            last_accessed: row.last_accessed,
        })
    }

    // ── Search ────────────────────────────────────────────────────────

    /// Full-text keyword search, returning ranked memory IDs.
    pub(super) async fn search_fts(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<FtsResult>, MemoryError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as::<_, IdRow>(
            "SELECT memory_id as id
             FROM memory_search_data
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
                id: MemoryId::from(r.id),
            })
            .collect())
    }

    /// Vector similarity search (KNN), returning ranked memory IDs.
    pub(super) async fn search_vector(
        &self,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<VecResult>, MemoryError> {
        let vector = Vector::from(query_embedding.to_vec());

        let rows = sqlx::query_as::<_, IdRow>(
            "SELECT memory_id as id
             FROM memory_search_data
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
                id: MemoryId::from(r.id),
            })
            .collect())
    }

    /// Check whether any embeddings exist.
    pub(super) async fn has_embeddings(&self) -> Result<bool, MemoryError> {
        let row = sqlx::query_as::<_, CountRow>(
            "SELECT COUNT(*) as count FROM memory_search_data WHERE embedding IS NOT NULL",
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

#[derive(sqlx::FromRow)]
struct AccessRow {
    last_accessed: Option<chrono::DateTime<chrono::Utc>>,
}

pub(super) struct AccessMetadata {
    pub(super) last_accessed: Option<chrono::DateTime<chrono::Utc>>,
}
