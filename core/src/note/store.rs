use es_entity::AtomicOperation;
use pgvector::Vector;
use sqlx::PgPool;

use crate::primitives::*;

use super::entity::Note;
use super::error::NoteError;

/// Search result returned by hybrid search.
#[derive(Debug, Clone)]
pub struct NoteSearchResult {
    pub id: NoteId,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
}

/// Manages the `note_search_data` auxiliary table for FTS + vector search.
#[derive(Clone)]
pub struct NoteSearchStore {
    pool: PgPool,
}

impl NoteSearchStore {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Insert or update the search data row for a note within an atomic op.
    #[tracing::instrument(name = "note.search_store.upsert_in_op", skip_all)]
    pub async fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        note: &Note,
    ) -> Result<(), NoteError> {
        let tags_json = serde_json::to_value(&note.tags).unwrap_or_default();
        sqlx::query(
            r#"INSERT INTO note_search_data (note_id, workspace_id, title_text, content_text, tags)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (note_id) DO UPDATE SET
                   title_text = EXCLUDED.title_text,
                   content_text = EXCLUDED.content_text,
                   tags = EXCLUDED.tags"#,
        )
        .bind(note.id)
        .bind(note.workspace_id)
        .bind(&note.title)
        .bind(&note.content)
        .bind(&tags_json)
        .execute(op.as_executor())
        .await?;
        Ok(())
    }

    /// Store a pre-computed embedding for a note.
    #[tracing::instrument(name = "note.search_store.set_embedding", skip_all)]
    pub async fn set_embedding(
        &self,
        note_id: NoteId,
        embedding: Vec<f32>,
    ) -> Result<(), NoteError> {
        let vec = Vector::from(embedding);
        sqlx::query("UPDATE note_search_data SET embedding = $1 WHERE note_id = $2")
            .bind(vec)
            .bind(note_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Hybrid search: FTS + vector similarity fused via Reciprocal Rank Fusion.
    #[tracing::instrument(name = "note.search_store.search", skip_all)]
    pub async fn search(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>, NoteError> {
        let over_fetch = (limit * 3).max(10) as i64;

        // FTS results
        let fts_rows: Vec<FtsRow> = sqlx::query_as(
            r#"SELECT note_id, title_text, content_text, tags,
                      ts_rank(search_tsv, plainto_tsquery('english', $1)) AS rank
               FROM note_search_data
               WHERE workspace_id = $2
                 AND search_tsv @@ plainto_tsquery('english', $1)
               ORDER BY rank DESC
               LIMIT $3"#,
        )
        .bind(query)
        .bind(workspace_id)
        .bind(over_fetch)
        .fetch_all(&self.pool)
        .await?;

        // Vector results (only if we have an embedding)
        let vec_rows: Vec<VecRow> = if let Some(emb) = query_embedding {
            let vec = Vector::from(emb);
            sqlx::query_as(
                r#"SELECT note_id, title_text, content_text, tags,
                          embedding <=> $1 AS distance
                   FROM note_search_data
                   WHERE workspace_id = $2
                     AND embedding IS NOT NULL
                   ORDER BY distance ASC
                   LIMIT $3"#,
            )
            .bind(vec)
            .bind(workspace_id)
            .bind(over_fetch)
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };

        // Reciprocal Rank Fusion (k = 60)
        let results = rrf_fuse(fts_rows, vec_rows, limit);
        Ok(results)
    }

}

// ---------------------------------------------------------------------------
// Internal types & RRF
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct FtsRow {
    note_id: NoteId,
    title_text: String,
    content_text: String,
    tags: serde_json::Value,
    #[allow(dead_code)]
    rank: f32,
}

#[derive(sqlx::FromRow)]
struct VecRow {
    note_id: NoteId,
    title_text: String,
    content_text: String,
    tags: serde_json::Value,
    #[allow(dead_code)]
    distance: f32,
}

fn parse_tags(val: &serde_json::Value) -> Vec<String> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Reciprocal Rank Fusion: combine two ranked lists into one scored list.
fn rrf_fuse(fts_rows: Vec<FtsRow>, vec_rows: Vec<VecRow>, limit: usize) -> Vec<NoteSearchResult> {
    use std::collections::HashMap;

    const K: f64 = 60.0;

    struct Candidate {
        title: String,
        content: String,
        tags: Vec<String>,
        score: f64,
    }

    let mut map: HashMap<NoteId, Candidate> = HashMap::new();

    for (rank, row) in fts_rows.into_iter().enumerate() {
        let entry = map.entry(row.note_id).or_insert_with(|| Candidate {
            title: row.title_text.clone(),
            content: row.content_text.clone(),
            tags: parse_tags(&row.tags),
            score: 0.0,
        });
        entry.score += 1.0 / (K + rank as f64 + 1.0);
    }

    for (rank, row) in vec_rows.into_iter().enumerate() {
        let entry = map.entry(row.note_id).or_insert_with(|| Candidate {
            title: row.title_text.clone(),
            content: row.content_text.clone(),
            tags: parse_tags(&row.tags),
            score: 0.0,
        });
        entry.score += 1.0 / (K + rank as f64 + 1.0);
    }

    let mut results: Vec<NoteSearchResult> = map
        .into_iter()
        .map(|(id, c)| NoteSearchResult {
            id,
            title: c.title,
            content: c.content,
            tags: c.tags,
            score: c.score,
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    results
}
