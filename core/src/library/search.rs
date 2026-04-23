use pgvector::Vector;
use sqlx::PgPool;

use super::error::LibraryError;
use super::file::{DocType, SearchableFields};

/// Search result returned by hybrid search.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
}

impl std::fmt::Display for SearchResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "id: {}\ntitle: {}\n", self.doc_id, self.title)?;
        if !self.tags.is_empty() {
            writeln!(f, "  tags: {}", self.tags.join(", "))?;
        }
        let preview: String = self.content.chars().take(200).collect();
        write!(f, "preview: {}", preview)
    }
}

/// Generic search store operating on `library_search_data`.
#[derive(Clone)]
pub(super) struct SearchStore {
    pool: PgPool,
}

impl SearchStore {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Insert or update the search data row for a document within an atomic op.
    #[tracing::instrument(name = "library.search_store.upsert_in_op", skip_all)]
    pub async fn upsert_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        fields: &SearchableFields,
    ) -> Result<(), LibraryError> {
        let tags_json = serde_json::to_value(&fields.tags).unwrap_or_default();
        let doc_type = fields.doc_type.as_str();
        sqlx::query(
            r#"INSERT INTO library_search_data (doc_id, doc_type, workspace_id, title_text, content_text, tags)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (doc_id, doc_type) DO UPDATE SET
                   title_text = EXCLUDED.title_text,
                   content_text = EXCLUDED.content_text,
                   tags = EXCLUDED.tags"#,
        )
        .bind(fields.doc_id)
        .bind(doc_type)
        .bind(fields.workspace_id)
        .bind(&fields.title)
        .bind(&fields.body)
        .bind(&tags_json)
        .execute(op.as_executor())
        .await?;
        Ok(())
    }

    /// Store a pre-computed embedding for a document.
    #[tracing::instrument(name = "library.search_store.set_embedding", skip_all)]
    pub async fn set_embedding(
        &self,
        doc_id: uuid::Uuid,
        doc_type: DocType,
        embedding: Vec<f32>,
    ) -> Result<(), LibraryError> {
        let vec = Vector::from(embedding);
        sqlx::query(
            "UPDATE library_search_data SET embedding = $1 WHERE doc_id = $2 AND doc_type = $3",
        )
        .bind(vec)
        .bind(doc_id)
        .bind(doc_type.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Hybrid search: FTS + vector similarity fused via Reciprocal Rank Fusion.
    #[tracing::instrument(name = "library.search_store.search", skip_all)]
    pub async fn search(
        &self,
        workspace_id: uuid::Uuid,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        doc_type: Option<DocType>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, LibraryError> {
        let over_fetch = (limit * 3).max(10) as i64;
        let doc_type_str = doc_type.map(|dt| dt.as_str().to_string());

        // FTS results
        let fts_rows: Vec<FtsRow> = sqlx::query_as(
            r#"SELECT doc_id, doc_type, title_text, content_text, tags,
                      ts_rank(search_tsv, plainto_tsquery('english', $1)) AS rank
               FROM library_search_data
               WHERE workspace_id = $2
                 AND search_tsv @@ plainto_tsquery('english', $1)
                 AND ($3::text IS NULL OR doc_type = $3)
               ORDER BY rank DESC
               LIMIT $4"#,
        )
        .bind(query)
        .bind(workspace_id)
        .bind(&doc_type_str)
        .bind(over_fetch)
        .fetch_all(&self.pool)
        .await?;

        // Vector results (only if we have an embedding)
        let vec_rows: Vec<VecRow> = if let Some(emb) = query_embedding {
            let vec = Vector::from(emb);
            sqlx::query_as(
                r#"SELECT doc_id, doc_type, title_text, content_text, tags,
                          embedding <=> $1 AS distance
                   FROM library_search_data
                   WHERE workspace_id = $2
                     AND embedding IS NOT NULL
                     AND ($3::text IS NULL OR doc_type = $3)
                   ORDER BY distance ASC
                   LIMIT $4"#,
            )
            .bind(vec)
            .bind(workspace_id)
            .bind(&doc_type_str)
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
    doc_id: uuid::Uuid,
    doc_type: String,
    title_text: String,
    content_text: String,
    tags: serde_json::Value,
    #[allow(dead_code)]
    rank: f32,
}

#[derive(sqlx::FromRow)]
struct VecRow {
    doc_id: uuid::Uuid,
    doc_type: String,
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

fn parse_doc_type(s: &str) -> DocType {
    match s {
        "note" => DocType::Note,
        _ => DocType::Note, // fallback
    }
}

/// Reciprocal Rank Fusion: combine two ranked lists into one scored list.
fn rrf_fuse(fts_rows: Vec<FtsRow>, vec_rows: Vec<VecRow>, limit: usize) -> Vec<SearchResult> {
    use std::collections::HashMap;

    const K: f64 = 60.0;

    struct Candidate {
        doc_type: DocType,
        title: String,
        content: String,
        tags: Vec<String>,
        score: f64,
    }

    let mut map: HashMap<(uuid::Uuid, String), Candidate> = HashMap::new();

    for (rank, row) in fts_rows.into_iter().enumerate() {
        let key = (row.doc_id, row.doc_type.clone());
        let entry = map.entry(key).or_insert_with(|| Candidate {
            doc_type: parse_doc_type(&row.doc_type),
            title: row.title_text.clone(),
            content: row.content_text.clone(),
            tags: parse_tags(&row.tags),
            score: 0.0,
        });
        entry.score += 1.0 / (K + rank as f64 + 1.0);
    }

    for (rank, row) in vec_rows.into_iter().enumerate() {
        let key = (row.doc_id, row.doc_type.clone());
        let entry = map.entry(key).or_insert_with(|| Candidate {
            doc_type: parse_doc_type(&row.doc_type),
            title: row.title_text.clone(),
            content: row.content_text.clone(),
            tags: parse_tags(&row.tags),
            score: 0.0,
        });
        entry.score += 1.0 / (K + rank as f64 + 1.0);
    }

    let mut results: Vec<SearchResult> = map
        .into_iter()
        .map(|((doc_id, _), c)| SearchResult {
            doc_id,
            doc_type: c.doc_type,
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
