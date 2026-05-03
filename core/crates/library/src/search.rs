use std::collections::HashMap;
use std::sync::Arc;

use es_entity::AtomicOperation;
use pgvector::Vector;
use sqlx::PgPool;

use crate::importer::DocType;
use crate::LibraryError;

#[derive(Debug, Clone)]
pub struct SearchableFields {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub scope_id: Option<uuid::Uuid>,
    pub scope_slug: Option<String>,
    pub name: String,
    pub path: Option<String>,
    pub content: String,
}

/// Result of a search query: the indexed fields plus the RRF score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub fields: SearchableFields,
    pub score: f64,
}

#[derive(Clone)]
pub struct SearchStore {
    pool: PgPool,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
}

impl SearchStore {
    pub(crate) fn new(
        pool: &PgPool,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
    ) -> Self {
        Self {
            pool: pool.clone(),
            embedder,
        }
    }

    #[tracing::instrument(name = "library.search_store.find_by_id", skip_all, fields(%doc_id, %doc_type))]
    pub async fn find_by_id(
        &self,
        doc_id: uuid::Uuid,
        doc_type: &DocType,
    ) -> Result<Option<SearchableFields>, LibraryError> {
        let row = sqlx::query!(
            r#"SELECT doc_id,
                      doc_type,
                      scope_id, scope_slug, name, path, content
               FROM library_documents
               WHERE doc_id = $1 AND doc_type = $2"#,
            doc_id,
            doc_type.as_str(),
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SearchableFields {
            doc_id: r.doc_id,
            doc_type: r.doc_type.into(),
            scope_id: r.scope_id,
            scope_slug: r.scope_slug,
            name: r.name,
            path: r.path,
            content: r.content,
        }))
    }

    #[tracing::instrument(name = "library.search_store.delete_in_op", skip_all, fields(%doc_id, %doc_type))]
    pub async fn delete_in_op(
        &self,
        op: &mut impl AtomicOperation,
        doc_id: uuid::Uuid,
        doc_type: DocType,
    ) -> Result<(), LibraryError> {
        sqlx::query!(
            "DELETE FROM library_documents WHERE doc_id = $1 AND doc_type = $2",
            doc_id,
            doc_type.as_str(),
        )
        .execute(op.as_executor())
        .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.search_store.upsert_in_op", skip_all)]
    pub async fn upsert_in_op(
        &self,
        op: &mut impl AtomicOperation,
        fields: &SearchableFields,
    ) -> Result<(), LibraryError> {
        sqlx::query!(
            r#"INSERT INTO library_documents
                   (doc_id, doc_type, scope_id, scope_slug, name, path, content)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               ON CONFLICT (doc_id, doc_type) DO UPDATE SET
                   scope_id   = EXCLUDED.scope_id,
                   scope_slug = EXCLUDED.scope_slug,
                   name       = EXCLUDED.name,
                   path       = EXCLUDED.path,
                   content    = EXCLUDED.content"#,
            fields.doc_id,
            fields.doc_type.as_str(),
            fields.scope_id,
            fields.scope_slug,
            fields.name,
            fields.path,
            fields.content,
        )
        .execute(op.as_executor())
        .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.search_store.set_embedding", skip_all, fields(%doc_id, %doc_type))]
    pub async fn set_embedding(
        &self,
        doc_id: uuid::Uuid,
        doc_type: &DocType,
        embedding: Vec<f32>,
    ) -> Result<(), LibraryError> {
        let vec = Vector::from(embedding);
        sqlx::query!(
            "UPDATE library_documents SET embedding = $1 WHERE doc_id = $2 AND doc_type = $3",
            vec as Vector,
            doc_id,
            doc_type.as_str(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reciprocal-Rank-Fusion of FTS and vector similarity. Filters:
    /// - `scope_ids` empty → no scope filter; non-empty → match scope_id
    ///   in the slice OR `scope_id IS NULL` (global / scopeless docs).
    /// - `doc_types` empty → no type filter; non-empty → match any.
    ///
    /// If `query_embedding` is `None` we embed the query string; if
    /// embedding fails we fall back to FTS-only.
    #[tracing::instrument(name = "library.search_store.search", skip_all)]
    pub async fn search(
        &self,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        scope_ids: &[uuid::Uuid],
        doc_types: &[DocType],
        limit: usize,
    ) -> Result<Vec<SearchHit>, LibraryError> {
        let doc_type_strs: Vec<&str> = doc_types.iter().map(|d| d.as_str()).collect();
        let scope_filter_active = !scope_ids.is_empty();
        let over_fetch = (limit * 3).max(10) as i64;

        let fts = sqlx::query!(
            r#"SELECT doc_id,
                      doc_type,
                      scope_id, scope_slug, name, path, content
               FROM library_documents
               WHERE search_tsv @@ plainto_tsquery('english', $1)
                 AND ($2::bool = false OR scope_id = ANY($3) OR scope_id IS NULL)
                 AND ($4::bool = false OR doc_type = ANY($5))
               ORDER BY ts_rank(search_tsv, plainto_tsquery('english', $1)) DESC
               LIMIT $6"#,
            query,
            scope_filter_active,
            scope_ids,
            !doc_type_strs.is_empty(),
            &doc_type_strs as &[&str],
            over_fetch,
        )
        .fetch_all(&self.pool)
        .await?;

        let embedding = match query_embedding {
            Some(v) => Some(v),
            None => match self.embedder.embed_query(query).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(error = %e, "embed_query failed; FTS-only");
                    None
                }
            },
        };

        let vec_rows = if let Some(emb) = embedding {
            let v = Vector::from(emb);
            sqlx::query!(
                r#"SELECT doc_id,
                          doc_type,
                          scope_id, scope_slug, name, path, content
                   FROM library_documents
                   WHERE embedding IS NOT NULL
                     AND ($2::bool = false OR scope_id = ANY($3) OR scope_id IS NULL)
                     AND ($4::bool = false OR doc_type = ANY($5))
                   ORDER BY embedding <=> $1
                   LIMIT $6"#,
                v as Vector,
                scope_filter_active,
                scope_ids,
                !doc_type_strs.is_empty(),
                &doc_type_strs as &[&str],
                over_fetch,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };

        // Reciprocal Rank Fusion of FTS + vector hits, normalised to
        // `[0, 1]`. With two lists, the per-list contribution at rank
        // 0 is `1/(K+1)`, so a doc ranked first in both lists scores
        // `2/(K+1)`. Dividing by that ceiling gives:
        //   - rank 0 in both lists → 1.0
        //   - rank 0 in one list   → 0.5
        // Higher ranks decay as expected.
        const K: f64 = 60.0;
        const N_LISTS: f64 = 2.0;
        let max_score = N_LISTS / (K + 1.0);
        let mut scores: HashMap<(uuid::Uuid, DocType), f64> = HashMap::new();
        let mut by_key: HashMap<(uuid::Uuid, DocType), SearchableFields> = HashMap::new();

        let mut absorb = |rank: usize, fields: SearchableFields| {
            let key = (fields.doc_id, fields.doc_type.clone());
            *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (K + rank as f64 + 1.0);
            by_key.entry(key).or_insert(fields);
        };

        for (rank, r) in fts.into_iter().enumerate() {
            absorb(
                rank,
                SearchableFields {
                    doc_id: r.doc_id,
                    doc_type: r.doc_type.into(),
                    scope_id: r.scope_id,
                    scope_slug: r.scope_slug,
                    name: r.name,
                    path: r.path,
                    content: r.content,
                },
            );
        }
        for (rank, r) in vec_rows.into_iter().enumerate() {
            absorb(
                rank,
                SearchableFields {
                    doc_id: r.doc_id,
                    doc_type: r.doc_type.into(),
                    scope_id: r.scope_id,
                    scope_slug: r.scope_slug,
                    name: r.name,
                    path: r.path,
                    content: r.content,
                },
            );
        }

        let mut ranked: Vec<((uuid::Uuid, DocType), f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        Ok(ranked
            .into_iter()
            .filter_map(|(key, score)| {
                by_key.remove(&key).map(|fields| SearchHit {
                    fields,
                    score: (score / max_score).clamp(0.0, 1.0),
                })
            })
            .collect())
    }

    /// Bulk hydration by `doc_id` only — `(doc_id, doc_type)` is the
    /// primary key but in practice `doc_id`s are globally unique
    /// (UUID-v5 derived per importer).
    #[tracing::instrument(name = "library.search_store.find_by_ids", skip_all, fields(count = ids.len()))]
    pub async fn find_by_ids(
        &self,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<SearchableFields>, LibraryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"SELECT doc_id, doc_type, scope_id, scope_slug, name, path, content
               FROM library_documents
               WHERE doc_id = ANY($1)"#,
            ids,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SearchableFields {
                doc_id: r.doc_id,
                doc_type: r.doc_type.into(),
                scope_id: r.scope_id,
                scope_slug: r.scope_slug,
                name: r.name,
                path: r.path,
                content: r.content,
            })
            .collect())
    }

    /// Removes every row whose `scope_id` matches the supplied id.
    /// Used by project/space cleanup paths to drop search rows in the
    /// same op as the entity delete.
    #[tracing::instrument(name = "library.search_store.delete_for_scope_in_op", skip_all, fields(%scope_id))]
    pub async fn delete_for_scope_in_op(
        &self,
        op: &mut impl AtomicOperation,
        scope_id: uuid::Uuid,
    ) -> Result<(), LibraryError> {
        sqlx::query!(
            "DELETE FROM library_documents WHERE scope_id = $1",
            scope_id,
        )
        .execute(op.as_executor())
        .await?;
        Ok(())
    }
}
