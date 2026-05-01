use std::collections::HashMap;
use std::sync::Arc;

use es_entity::AtomicOperation as _;
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
        op: &mut es_entity::DbOp<'_>,
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
        op: &mut es_entity::DbOp<'_>,
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

    /// Reciprocal-Rank-Fusion of FTS and vector similarity. If
    /// `query_embedding` is `None` we embed the query string; if embedding
    /// fails we fall back to FTS-only.
    #[tracing::instrument(name = "library.search_store.search", skip_all)]
    pub async fn search(
        &self,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        doc_type: Option<DocType>,
        limit: usize,
    ) -> Result<Vec<SearchableFields>, LibraryError> {
        let doc_type_filter: Option<&str> = doc_type.as_ref().map(|d| d.as_str());
        let over_fetch = (limit * 3).max(10) as i64;

        let fts = sqlx::query!(
            r#"SELECT doc_id,
                      doc_type,
                      scope_id, scope_slug, name, path, content
               FROM library_documents
               WHERE search_tsv @@ plainto_tsquery('english', $1)
                 AND ($2::text IS NULL OR doc_type = $2)
               ORDER BY ts_rank(search_tsv, plainto_tsquery('english', $1)) DESC
               LIMIT $3"#,
            query,
            doc_type_filter,
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
                     AND ($2::text IS NULL OR doc_type = $2)
                   ORDER BY embedding <=> $1
                   LIMIT $3"#,
                v as Vector,
                doc_type_filter,
                over_fetch,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };

        const K: f64 = 60.0;
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
            .filter_map(|(key, _)| by_key.remove(&key))
            .collect())
    }
}
